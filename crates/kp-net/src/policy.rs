// Server-authoritative principal and typed resource actions follow omnigraph-policy (MIT).
pub use kp_model::PolicyAction as Action;
use kp_model::{ActorId, LibraryMode, MemberRole};

#[derive(Clone, Debug)]
pub struct Principal {
    pub actor: ActorId,
    pub role: MemberRole,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

pub fn authorize(
    principal: Option<&Principal>,
    mode: LibraryMode,
    action: Action,
    author: Option<&ActorId>,
) -> Decision {
    let Some(principal) = principal else {
        return Decision::Deny;
    };
    let moderator = matches!(principal.role, MemberRole::Owner | MemberRole::Maintainer);
    let allowed = match action {
        Action::Read | Action::Synthesize | Action::AdoptSkill => true,
        Action::Publish => moderator || author == Some(&principal.actor),
        Action::Withdraw | Action::Purge => {
            mode == LibraryMode::Personal || moderator || author == Some(&principal.actor)
        }
        Action::ManageMembers => moderator,
    };
    if allowed {
        Decision::Allow
    } else {
        Decision::Deny
    }
}
