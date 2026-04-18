//! sam-claude — Claude API client and conversation management for Sam.
//!
//! M1 provided the [`probe::claude_version`] health check and [`cli::ClaudeCli`]
//! stub. M3 adds the direct Claude Messages API integration: client, types,
//! API key loading, token budget tracking, and session management.

pub mod api;
pub mod api_key;
pub mod budget;
pub mod cli;
pub mod probe;
pub mod prompt;
pub mod session;
pub mod tools;
pub mod types;

// ── Re-exports ──────────────────────────────────────────────────────────

pub use api::SamClaudeClient;
pub use api_key::load_api_key;
pub use budget::TokenBudget;
pub use cli::{ClaudeCli, ClaudeSpawnRequest};
pub use probe::claude_version;
pub use prompt::load_system_prompt;
pub use session::ConversationSession;
pub use types::{ChatMessage, ChatResponse};
