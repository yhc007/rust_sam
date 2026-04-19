//! LLM backend trait — abstracts over Anthropic, OpenAI-compatible, etc.

use async_trait::async_trait;

use crate::types::{ChatMessage, ChatResponse, ToolDefinition};

/// Trait for LLM backends that can power Sam's conversation loop.
///
/// Implementations translate between Sam's internal types and the
/// provider-specific wire format.
#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn chat(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> anyhow::Result<ChatResponse>;
}
