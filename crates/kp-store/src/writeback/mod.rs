//! Viewer-scoped lossless Markdown writeback (`kp apply`) and `writeback_proposal` resolution.
//!
//! Enforces strict caller authentication, frontmatter viewer/library matching, `.kp-export.lock`
//! directory serialization, pre-persistence [`SecretRedactor`] scrubbing on all user-supplied text,
//! `RawOnly` zero-cloud-embedder isolation, and zero silent text loss on stale baselines,
//! derived insights, cross-author member edits, or freeform additions.

mod apply_dir;
mod apply_proposal;
mod parser;
mod tx_ops;

use kp_model::{
    ActorId, DerivationView, Fact, FactAccess, FactId, LibraryId, ModelRoutePolicy, ReviewId,
    SourceId,
};
use serde::{Deserialize, Serialize};

/// Classification of a [`ReviewKind::WritebackProposal`] review item payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WritebackProposalOp {
    EditFact,
    DeleteFact,
    FreeformBlock,
}

impl WritebackProposalOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EditFact => "edit_fact",
            Self::DeleteFact => "delete_fact",
            Self::FreeformBlock => "freeform_block",
        }
    }
}

/// Record of an in-place base fact replacement performed during [`Store::apply_markdown_directory`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WritebackFactUpdate {
    pub old_fact_id: FactId,
    pub new_fact_id: FactId,
    pub source_id: SourceId,
}

/// Summary returned by [`Store::apply_markdown_directory`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyReport {
    pub library_id: LibraryId,
    pub viewer: ActorId,
    pub base_seq: i64,
    pub current_seq: i64,
    pub unchanged: bool,
    pub baseline_conflict: bool,
    pub updated_facts: Vec<WritebackFactUpdate>,
    pub deleted_fact_ids: Vec<FactId>,
    pub enqueued_proposals: Vec<ReviewId>,
}

pub(super) fn default_policy_from_env() -> ModelRoutePolicy {
    if let Ok(raw) = std::env::var("KP_MODEL_POLICY") {
        if matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "raw_only" | "raw-only" | "none"
        ) {
            return ModelRoutePolicy::RawOnly;
        }
    }
    ModelRoutePolicy::HostCli
}

pub(super) fn merge_route_policy(
    primary: Option<&ModelRoutePolicy>,
    fallback: &ModelRoutePolicy,
) -> ModelRoutePolicy {
    if matches!(primary, Some(ModelRoutePolicy::RawOnly))
        || matches!(fallback, ModelRoutePolicy::RawOnly)
    {
        ModelRoutePolicy::RawOnly
    } else {
        primary.cloned().unwrap_or_else(|| fallback.clone())
    }
}

pub(crate) fn is_base_fact_readable_by_actor(
    graph: &DerivationView,
    fact: &Fact,
    actor: &ActorId,
) -> bool {
    let FactAccess::Base { audience, .. } = &fact.access else {
        return false;
    };
    if !audience.allows(actor, &graph.members) {
        return false;
    }
    if let Some(scope_id) = &fact.source_scope_id {
        let Some(scope_aud) = graph.source_scopes.get(scope_id) else {
            return false;
        };
        if !scope_aud.allows(actor, &graph.members) {
            return false;
        }
    }
    true
}
