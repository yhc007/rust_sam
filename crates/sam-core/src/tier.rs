//! Privilege tiers for Sam actions.

use serde::{Deserialize, Serialize};

/// The privilege level required for an action.
///
/// `Chat` actions are pure conversation (no tool invocation).
/// `Tier1`..`Tier3` escalate scope — `Tier1` covers self-configuration
/// (prompts, handles, budget), `Tier2` covers code / session spawn, and
/// `Tier3` covers writes to external systems (Notion, filesystem outside
/// `~/.sam`). `Destructive` always requires explicit owner confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Chat,
    Tier1,
    Tier2,
    Tier3,
    Destructive,
}

impl Tier {
    /// Short label used in status output and logs.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Tier1 => "tier1",
            Self::Tier2 => "tier2",
            Self::Tier3 => "tier3",
            Self::Destructive => "destructive",
        }
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl std::str::FromStr for Tier {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "chat" => Ok(Self::Chat),
            "tier1" | "1" => Ok(Self::Tier1),
            "tier2" | "2" => Ok(Self::Tier2),
            "tier3" | "3" => Ok(Self::Tier3),
            "destructive" | "destroy" => Ok(Self::Destructive),
            other => Err(format!("unknown tier: {other}")),
        }
    }
}
