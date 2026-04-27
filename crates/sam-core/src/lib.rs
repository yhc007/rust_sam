//! sam-core — shared types, configuration, paths, and errors for Sam.
//!
//! This is a leaf crate that other `sam-*` crates build on. It deliberately
//! keeps its dependency surface minimal.

pub mod agent;
pub mod config;
pub mod cron_store;
pub mod delivery_queue;
pub mod error;
pub mod flow;
pub mod flow_store;
pub mod hot_reload;
pub mod paths;
pub mod plugin;
pub mod skill_store;
pub mod tier;

pub use agent::{AgentDef, AgentStore, ToolFilter, agents_dir};
pub use config::{load_config, AgentConfig, BrowserConfig, ClaudeCodeConfig, HeartbeatConfig,
    IMessageConfig, IdentityConfig, LlmConfig, McpConfig, McpServerConfig, MemoryConfig,
    NotionConfig, SafetyConfig, SamConfig, TelegramConfig, TwitterConfig, WebSearchConfig,
    WhisperConfig};
pub use cron_store::{CronJob, CronSchedule, CronStore, cron_matches, new_job, parse_datetime_to_unix};
pub use delivery_queue::{DeliveryQueue, QueuedMessage};
pub use error::SamError;
pub use flow::{FlowDef, FlowStep, FlowTrigger};
pub use flow_store::{FlowStore, flows_dir};
pub use hot_reload::{run_hot_reload, SharedConfig};
pub use paths::{config_path, expand_tilde, prompts_dir, sam_home, state_dir, tools_dir};
pub use plugin::{Plugin, PluginManifest, PluginStore, plugins_dir};
pub use skill_store::{CustomSkill, SkillExec, SkillStore, interpolate_args};
pub use tier::Tier;

/// Convenience alias for crate-level results.
pub type Result<T> = std::result::Result<T, SamError>;
