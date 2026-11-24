use kp_model::{ActorId, EpochMs, FactId, FactKind};
use kp_store::Store;

use crate::Result;

pub use kp_model::{SkillPackage, SkillSlug, EVIDENCE_BOUNDARY, READ_ONLY_SKILL};

/// Retry eligibility after episode commit, including recovery from interrupted materialization.
pub fn promote_ready(store: &mut Store, actor: &ActorId, at: EpochMs) -> Result<Vec<FactId>> {
    let bullets = store
        .visible_facts(actor, at)?
        .into_iter()
        .filter(|f| {
            f.kind == FactKind::PlaybookBullet
                && f.author == *actor
                && f.origin_library == *store.library_id()
        })
        .collect::<Vec<_>>();
    let mut promoted = Vec::new();
    for bullet in bullets {
        if let Some(id) = store.promote_skill(&bullet.id, actor, at)? {
            promoted.push(id);
        }
    }
    Ok(promoted)
}
