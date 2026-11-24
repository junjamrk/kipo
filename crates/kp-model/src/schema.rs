use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{KpError, Result};

macro_rules! text_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $text)] $variant),+
        }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.as_str()) }
        }

        impl FromStr for $name {
            type Err = KpError;
            fn from_str(value: &str) -> Result<Self> {
                match value {
                    $($text => Ok(Self::$variant)),+,
                    _ => Err(KpError::invalid(stringify!($name), "unknown value")),
                }
            }
        }
    };
}

pub(crate) use text_enum;

text_enum!(LibraryMode { Personal => "personal", Community => "community" });
text_enum!(MemberRole { Owner => "owner", Maintainer => "maintainer", Member => "member" });
text_enum!(Cardinality { Single => "single", Multi => "multi" });
text_enum!(Mutability { Fixed => "fixed", TimeVarying => "time_varying" });
text_enum!(PolicyAction {
    Read => "read", Publish => "publish", Withdraw => "withdraw", Synthesize => "synthesize",
    AdoptSkill => "adopt_skill", Purge => "purge", ManageMembers => "manage_members"
});

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotContract {
    pub predicate: String,
    pub namespace: String,
    pub cardinality: Cardinality,
    pub mutability: Mutability,
    pub value_kind: String,
    pub description: String,
    pub instruction: String,
}

pub fn core_lexicon() -> Vec<SlotContract> {
    use Cardinality::{Multi, Single};
    use Mutability::{Fixed, TimeVarying};

    [
        (
            "kp.core.preference",
            "kp.core",
            Multi,
            TimeVarying,
            "A stated preference",
            "Preserve the speaker and scope.",
        ),
        (
            "kp.core.constraint",
            "kp.core",
            Multi,
            TimeVarying,
            "A requirement or prohibition",
            "Retain conditions and exceptions.",
        ),
        (
            "kp.core.weather",
            "kp.core",
            Single,
            TimeVarying,
            "Weather at a place and time",
            "Bind the subject to a location and supply a validity interval.",
        ),
        (
            "kp.core.schedule",
            "kp.core",
            Multi,
            TimeVarying,
            "A scheduled event",
            "Preserve time zone and participants.",
        ),
        (
            "kp.core.decision",
            "kp.core",
            Multi,
            Fixed,
            "An explicit decision",
            "Preserve rationale and author attribution.",
        ),
        (
            "kp.code.convention",
            "kp.code",
            Multi,
            TimeVarying,
            "A repository convention",
            "Scope the claim to its repository.",
        ),
        (
            "kp.code.pitfall",
            "kp.code",
            Multi,
            TimeVarying,
            "A demonstrated code pitfall",
            "Retain a reproducible failure and code anchor.",
        ),
        (
            "kp.code.api_contract",
            "kp.code",
            Single,
            TimeVarying,
            "A symbol's API contract",
            "Identify the symbol and signature version.",
        ),
    ]
    .into_iter()
    .map(
        |(predicate, namespace, cardinality, mutability, description, instruction)| SlotContract {
            predicate: predicate.into(),
            namespace: namespace.into(),
            cardinality,
            mutability,
            value_kind: "json".into(),
            description: description.into(),
            instruction: instruction.into(),
        },
    )
    .collect()
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
