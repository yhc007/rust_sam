//! sam-core — shared types, configuration, paths, and errors for Sam.
//!
//! This is a leaf crate that other `sam-*` crates build on. It deliberately
//! keeps its dependency surface minimal.

pub mod config;
pub mod cron_store;
pub mod delivery_queue;
pub mod error;
pub mod paths;
pub mod tier;

pub use config::{load_config, ClaudeCodeConfig, IMessageConfig, IdentityConfig, LlmConfig,
    MemoryConfig, NotionConfig, SafetyConfig, SamConfig, TelegramConfig, TwitterConfig,
    WebSearchConfig};
pub use cron_store::{CronJob, CronSchedule, CronStore, new_job, parse_datetime_to_unix};
pub use delivery_queue::{DeliveryQueue, QueuedMessage};
pub use error::SamError;
pub use paths::{config_path, expand_tilde, prompts_dir, sam_home, state_dir, tools_dir};
pub use tier::Tier;

/// Convenience alias for crate-level results.
pub type Result<T> = std::result::Result<T, SamError>;
