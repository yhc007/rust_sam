//! sam-claude — LLM API client and conversation management for Sam.
//!
//! Supports multiple LLM providers (Anthropic Claude, xAI Grok) via the
//! [`LlmClient`] trait. Provider selection is driven by `config.llm.provider`.

pub mod api;
pub mod api_key;
pub mod backend;
pub mod budget;
pub mod cli;
pub mod flow_runner;
pub mod llm_client;
pub mod mcp;
pub mod openai_client;
pub mod probe;
pub mod prompt;
pub mod session;
pub mod tools;
pub mod types;
pub mod whisper;
pub mod xai;

// ── Re-exports ──────────────────────────────────────────────────────────

pub use api::SamClaudeClient;
pub use api_key::load_api_key;
pub use backend::LlmBackend;
pub use budget::TokenBudget;
pub use cli::{ClaudeCli, ClaudeSpawnRequest};
pub use flow_runner::{run_flow, FlowResult};
pub use llm_client::LlmClient;
pub use openai_client::OpenAiCompatibleClient;
pub use probe::claude_version;
pub use prompt::load_system_prompt;
pub use session::ConversationSession;
pub use types::{ChatMessage, ChatResponse};
pub use xai::XaiClient;
