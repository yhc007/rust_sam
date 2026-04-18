//! Conversation session — manages history, budget, and Claude round-trips
//! with tool_use support.

use tracing::{info, warn};

use sam_core::SamConfig;
use sam_memory_adapter::MemoryAdapter;

use crate::api::SamClaudeClient;
use crate::budget::TokenBudget;
use crate::tools::{builtin_tool_definitions, execute_builtin, ToolContext, MAX_TOOL_ROUNDS};
use crate::types::*;

/// A single conversation session identified by an iMessage handle.
pub struct ConversationSession {
    /// iMessage handle or identifier for this session.
    pub handle: String,
    /// Conversation history (user + assistant turns). System prompt is
    /// kept separate and not included here.
    history: Vec<ChatMessage>,
    /// Maximum number of messages to retain in history.
    max_history: usize,
    /// System prompt sent with every API call.
    system_prompt: String,
    /// Tool definitions sent with every API call.
    tools: Vec<ToolDefinition>,
}

impl ConversationSession {
    /// Create a new conversation session.
    pub fn new(handle: &str, system_prompt: String, max_history: usize) -> Self {
        Self {
            handle: handle.to_string(),
            history: Vec::new(),
            max_history,
            system_prompt,
            tools: builtin_tool_definitions(),
        }
    }

    /// Send a user message and return the assistant's reply.
    ///
    /// Runs an agentic loop: if Claude responds with tool_use, the tools
    /// are executed and the results fed back until Claude produces a final
    /// text response (or the loop limit is reached).
    pub async fn reply(
        &mut self,
        client: &SamClaudeClient,
        budget: &mut TokenBudget,
        user_text: &str,
        memory: Option<&mut MemoryAdapter>,
        config: &SamConfig,
    ) -> anyhow::Result<String> {
        // Append user message.
        self.history.push(ChatMessage::text("user", user_text));

        let tools_slice = &self.tools;
        let mut total_input = 0u32;
        let mut total_output = 0u32;

        let mut mem_opt = memory;

        for round in 0..MAX_TOOL_ROUNDS {
            let resp = client
                .chat(
                    &self.system_prompt,
                    &self.history,
                    Some(tools_slice),
                )
                .await?;

            total_input += resp.input_tokens;
            total_output += resp.output_tokens;

            if resp.stop_reason == "tool_use" && !resp.tool_calls.is_empty() {
                info!(
                    round = round,
                    tools = resp.tool_calls.len(),
                    "tool_use round"
                );

                // Build the assistant message with the full content blocks.
                let mut blocks: Vec<ContentBlock> = Vec::new();
                if !resp.text.is_empty() {
                    blocks.push(ContentBlock::Text {
                        text: resp.text.clone(),
                    });
                }
                for tc in &resp.tool_calls {
                    blocks.push(ContentBlock::ToolUse {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        input: tc.input.clone(),
                    });
                }
                self.history.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: MessageContent::Blocks(blocks),
                });

                // Execute each tool and build tool_result messages.
                for tc in &resp.tool_calls {
                    let mut ctx = ToolContext {
                        memory: mem_opt.as_deref_mut(),
                        config,
                    };
                    let result = execute_builtin(&tc.name, &tc.input, &mut ctx).await;
                    let (result_text, is_error) = match result {
                        Ok(text) => (text, false),
                        Err(err) => (err, true),
                    };
                    info!(
                        tool = %tc.name,
                        is_error = is_error,
                        "tool result"
                    );
                    self.history
                        .push(ChatMessage::tool_result(&tc.id, &result_text, is_error));
                }

                continue;
            }

            // stop_reason is "end_turn" (or max_tokens, etc.) — we have the final text.
            let total_tokens = total_input + total_output;
            if let Err(_e) = budget.check_and_record(total_tokens) {
                self.history.pop();
                return Ok("오늘 토큰 한도에 도달했어. 내일 다시 이야기하자!".to_string());
            }

            if let Err(e) = budget.save() {
                warn!(error = %e, "failed to persist token budget");
            }

            info!(
                handle = %self.handle,
                input_tokens = total_input,
                output_tokens = total_output,
                rounds = round + 1,
                remaining = budget.remaining(),
                "conversation turn complete"
            );

            // Append final assistant message.
            self.history.push(ChatMessage::text("assistant", &resp.text));
            self.trim_history();

            // Auto-store conversation in memory.
            if let Some(mem) = mem_opt.as_deref_mut() {
                if let Err(e) = mem.store_conversation(&self.handle, user_text, &resp.text) {
                    warn!(error = %e, "failed to store conversation memory");
                }
            }

            return Ok(resp.text);
        }

        warn!(
            handle = %self.handle,
            "exhausted tool rounds ({MAX_TOOL_ROUNDS})"
        );
        Ok("도구 호출 한도에 도달했어. 다시 시도해줘.".to_string())
    }

    /// Remove oldest messages to stay within `max_history`.
    fn trim_history(&mut self) {
        if self.history.len() > self.max_history {
            let excess = self.history.len() - self.max_history;
            self.history.drain(..excess);
        }
    }

    /// Read-only access to current history.
    pub fn history(&self) -> &[ChatMessage] {
        &self.history
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_keeps_max_history() {
        let mut session = ConversationSession::new(
            "+821012345678",
            "system prompt".to_string(),
            4,
        );

        for i in 0..6 {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            session.history.push(ChatMessage::text(role, &format!("message {i}")));
        }

        assert_eq!(session.history.len(), 6);
        session.trim_history();
        assert_eq!(session.history.len(), 4);

        match &session.history[0].content {
            MessageContent::Text(s) => assert_eq!(s, "message 2"),
            _ => panic!("expected text"),
        }
        match &session.history[3].content {
            MessageContent::Text(s) => assert_eq!(s, "message 5"),
            _ => panic!("expected text"),
        }
    }
}
