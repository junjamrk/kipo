//! Store mutations maintain proofs in the same transaction as the premise transition.
use kp_model::{ActorId, EpochMs, Fact, FactId};
use kp_store::{Result, Store};

pub use crate::rules::verify_hard_rules_match_full_recompute;

pub fn withdraw(
    store: &mut Store,
    target: &str,
    actor: &ActorId,
    at: EpochMs,
) -> Result<Vec<FactId>> {
    store.forget(target, actor, at)
}

pub fn supersede(store: &mut Store, old: &FactId, replacement: &Fact) -> Result<()> {
    store.supersede(old, replacement)
}

pub fn expire(store: &mut Store, at: EpochMs) -> Result<()> {
    store.expire(at)
}
