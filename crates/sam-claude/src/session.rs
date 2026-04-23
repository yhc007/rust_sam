//! Conversation session — manages history, budget, and Claude round-trips
//! with tool_use support.

use tracing::{info, warn};

use sam_core::SamConfig;
use sam_memory_adapter::MemoryAdapter;

use crate::budget::TokenBudget;
use crate::llm_client::LlmClient;
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
        client: &dyn LlmClient,
        budget: &mut TokenBudget,
        user_text: &str,
        memory: Option<&mut MemoryAdapter>,
        config: &SamConfig,
    ) -> anyhow::Result<String> {
        // Trim history *before* adding the new message to prevent unbounded growth.
        self.trim_history();

        // Append user message.
        self.history.push(ChatMessage::text("user", user_text));

        let tools_slice = &self.tools;
        let mut total_input = 0u32;
        let mut total_output = 0u32;

        let mut mem_opt = memory;

        for round in 0..MAX_TOOL_ROUNDS {
            let resp = match client
                .chat(
                    &self.system_prompt,
                    &self.history,
                    Some(tools_slice),
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    // On API error, remove the user message we just added
                    // so the history doesn't grow unboundedly with failed turns.
                    self.history.pop();
                    return Err(e);
                }
            };

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
    ///
    /// After trimming, ensures the history doesn't start with orphaned
    /// tool_result or assistant tool_use messages (which would cause Claude
    /// API errors due to missing tool_use/tool_result pairs).
    fn trim_history(&mut self) {
        if self.history.len() > self.max_history {
            let excess = self.history.len() - self.max_history;
            self.history.drain(..excess);
        }

        // Drop leading messages until we start with a clean user text or
        // assistant text message (not a tool_result or tool_use block).
        while !self.history.is_empty() && self.is_tool_message(0) {
            self.history.remove(0);
        }
    }

    /// Check if the message at `index` is a tool-related message
    /// (tool_result from user, or assistant message containing tool_use blocks).
    fn is_tool_message(&self, index: usize) -> bool {
        let msg = &self.history[index];
        match &msg.content {
            MessageContent::Text(_) => false,
            MessageContent::Blocks(blocks) => blocks.iter().any(|b| {
                matches!(
                    b,
                    ContentBlock::ToolResult { .. } | ContentBlock::ToolUse { .. }
                )
            }),
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

    #[test]
    fn trim_drops_orphaned_tool_messages() {
        let mut session = ConversationSession::new(
            "+821012345678",
            "system prompt".to_string(),
            4,
        );

        // Simulate: user, assistant(tool_use), user(tool_result), assistant(text), user, assistant
        session.history.push(ChatMessage::text("user", "msg 0"));
        session.history.push(ChatMessage {
            role: "assistant".to_string(),
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "toolu_01".to_string(),
                name: "current_time".to_string(),
                input: serde_json::json!({}),
            }]),
        });
        session.history.push(ChatMessage::tool_result("toolu_01", "12:00", false));
        session.history.push(ChatMessage::text("assistant", "It's noon"));
        session.history.push(ChatMessage::text("user", "msg 4"));
        session.history.push(ChatMessage::text("assistant", "msg 5"));

        // max_history = 4, so naive trim would remove first 2,
        // leaving tool_result at front → API error.
        session.trim_history();

        // Should have dropped the orphaned tool_result too.
        assert!(!session.history.is_empty());
        match &session.history[0].content {
            MessageContent::Text(s) => assert_eq!(s, "It's noon"),
            _ => panic!("first message should be clean text, got: {:?}", session.history[0]),
        }
    }
}
