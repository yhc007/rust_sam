//! Flow data model — TOML-defined multi-step pipelines.
//!
//! A flow is a sequence of steps that Sam can execute, either on demand
//! (manual trigger) or on a schedule (cron trigger).

use serde::{Deserialize, Serialize};

/// A complete flow definition parsed from a TOML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowDef {
    /// Unique name for this flow (derived from filename).
    pub name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// How this flow is triggered.
    #[serde(default)]
    pub trigger: FlowTrigger,
    /// Ordered list of steps to execute.
    pub steps: Vec<FlowStep>,
}

/// How a flow is triggered.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum FlowTrigger {
    /// Triggered manually via the `run_flow` tool.
    Manual,
    /// Triggered on a cron schedule (5-field cron expression).
    Cron { expr: String },
}

impl Default for FlowTrigger {
    fn default() -> Self {
        Self::Manual
    }
}

/// A single step in a flow pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum FlowStep {
    /// Call the LLM with a prompt (supports variable interpolation).
    Llm {
        name: String,
        prompt: String,
        #[serde(default = "default_max_tokens")]
        max_tokens: u32,
    },
    /// Execute a built-in tool.
    Tool {
        name: String,
        tool: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    /// Send a message to a handle (iMessage/Telegram).
    Send {
        name: String,
        handle: String,
        body: String,
    },
}

fn default_max_tokens() -> u32 {
    1024
}

impl FlowStep {
    /// Get the step name (used for variable interpolation).
    pub fn step_name(&self) -> &str {
        match self {
            FlowStep::Llm { name, .. } => name,
            FlowStep::Tool { name, .. } => name,
            FlowStep::Send { name, .. } => name,
        }
    }
}
