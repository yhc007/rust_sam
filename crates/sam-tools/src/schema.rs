//! TOML shape for `~/.sam/tools/*.toml` files.

use sam_core::Tier;
use serde::{Deserialize, Serialize};

/// Raw `[command]` block pulled straight from the tool TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawCommand {
    /// Executable name or path (required).
    pub program: String,
    /// Argument template list; placeholders like `{{input.foo}}` are resolved
    /// at call time by future tool executors. M1 stores them verbatim.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional working directory. Tilde-expansion happens at call time.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Optional per-invocation timeout, in seconds.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// A discovered tool definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    /// Stable identifier, unique within the registry.
    pub name: String,
    /// Short, user-facing description.
    pub description: String,
    /// Privilege tier required to run this tool.
    pub tier: Tier,
    /// Command template.
    pub command: RawCommand,
    /// Raw JSON input schema, stored verbatim. Parsing / validation is
    /// handled by M2.
    #[serde(default)]
    pub input_schema_raw: String,
}

impl ToolDef {
    /// Parse from a tool-file TOML string.
    pub fn from_toml(raw: &str) -> anyhow::Result<Self> {
        #[derive(Debug, Deserialize)]
        struct OnDisk {
            name: String,
            description: String,
            tier: Tier,
            command: RawCommand,
            #[serde(default)]
            input_schema: Option<toml::Value>,
        }
        let parsed: OnDisk = toml::from_str(raw)?;
        let input_schema_raw = match parsed.input_schema {
            Some(v) => serde_json::to_string(&v)?,
            None => String::new(),
        };
        Ok(Self {
            name: parsed.name,
            description: parsed.description,
            tier: parsed.tier,
            command: parsed.command,
            input_schema_raw,
        })
    }
}
