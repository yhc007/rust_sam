//! Conversation session — manages history, budget, and Claude round-trips
//! with tool_use support.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{info, warn};

use sam_core::{state_dir, CronStore, FlowStore, SamConfig};
use sam_memory_adapter::MemoryAdapter;

use crate::budget::TokenBudget;
use crate::llm_client::LlmClient;
use crate::mcp::McpClient;
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
    /// Rolling summary of dropped conversation history (context compaction).
    context_summary: Option<String>,
    /// Handoff context from a previous agent — injected into system prompt
    /// so the receiving agent has full context of the prior conversation.
    handoff_context: Option<String>,
}

/// Serializable snapshot of session state for persistence.
#[derive(serde::Serialize, serde::Deserialize)]
struct SessionSnapshot {
    history: Vec<ChatMessage>,
    context_summary: Option<String>,
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
            context_summary: None,
            handoff_context: None,
        }
    }

    /// Create a session with a filtered set of built-in tools.
    /// `filter` determines which tools from `builtin_tool_definitions()` are included.
    pub fn new_with_filter(
        handle: &str,
        system_prompt: String,
        max_history: usize,
        filter: &sam_core::ToolFilter,
    ) -> Self {
        let tools: Vec<ToolDefinition> = builtin_tool_definitions()
            .into_iter()
            .filter(|t| filter.allows(&t.name))
            .collect();
        Self {
            handle: handle.to_string(),
            history: Vec::new(),
            max_history,
            system_prompt,
            tools,
            context_summary: None,
            handoff_context: None,
        }
    }

    /// Load a session from disk, falling back to a fresh session if the file
    /// doesn't exist or cannot be parsed.
    pub fn load(handle: &str, system_prompt: String, max_history: usize) -> Self {
        let path = Self::session_path(handle);
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(data) => match serde_json::from_str::<SessionSnapshot>(&data) {
                    Ok(snap) => {
                        info!(handle = %handle, path = %path.display(), "restored session from disk");
                        return Self {
                            handle: handle.to_string(),
                            history: snap.history,
                            max_history,
                            system_prompt,
                            tools: builtin_tool_definitions(),
                            context_summary: snap.context_summary,
                            handoff_context: None,
                        };
                    }
                    Err(e) => {
                        warn!(handle = %handle, error = %e, "failed to parse session file, starting fresh");
                    }
                },
                Err(e) => {
                    warn!(handle = %handle, error = %e, "failed to read session file, starting fresh");
                }
            }
        }
        Self::new(handle, system_prompt, max_history)
    }

    /// Persist the current session (history + context_summary) to disk.
    pub fn save(&self) {
        let path = Self::session_path(&self.handle);
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                warn!(error = %e, "failed to create sessions directory");
                return;
            }
        }
        let snap = SessionSnapshot {
            history: self.history.clone(),
            context_summary: self.context_summary.clone(),
        };
        match serde_json::to_string(&snap) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    warn!(error = %e, "failed to write session file");
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to serialize session");
            }
        }
    }

    /// Return the filesystem path for a given handle's session file.
    fn session_path(handle: &str) -> PathBuf {
        let sanitized: String = handle
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        state_dir().join("sessions").join(format!("{sanitized}.json"))
    }

    /// Send a user message and return the assistant's reply.
    ///
    /// Runs an agentic loop: if Claude responds with tool_use, the tools
    /// are executed and the results fed back until Claude produces a final
    /// text response (or the loop limit is reached).
    ///
    /// `images` contains (mime_type, base64_data) pairs for any image
    /// attachments to include with the user message (Claude Vision).
    #[allow(clippy::too_many_arguments)]
    pub async fn reply(
        &mut self,
        client: &dyn LlmClient,
        budget: &mut TokenBudget,
        user_text: &str,
        images: &[(String, String)],
        memory: Option<&mut MemoryAdapter>,
        config: &SamConfig,
        cron_store: Option<Arc<Mutex<CronStore>>>,
        flow_store: Option<Arc<Mutex<FlowStore>>>,
        mcp_clients: Option<Arc<Mutex<Vec<McpClient>>>>,
        skill_store: Option<Arc<Mutex<sam_core::SkillStore>>>,
    ) -> anyhow::Result<String> {
        // Trim history *before* adding the new message to prevent unbounded growth.
        // Compact dropped messages into context_summary.
        let dropped = self.trim_history();
        if !dropped.is_empty() {
            self.compact(client, &dropped).await;
        }

        // Append user message (with images if present).
        if images.is_empty() {
            self.history.push(ChatMessage::text("user", user_text));
        } else {
            self.history.push(ChatMessage::user_with_images(user_text, images));
        }

        // Build effective system prompt (base + context summary).
        let effective_system = self.effective_system_prompt();

        let tools_slice = &self.tools;
        let mut total_input = 0u32;
        let mut total_output = 0u32;

        let mut mem_opt = memory;

        for round in 0..MAX_TOOL_ROUNDS {
            let resp = match client
                .chat(
                    &effective_system,
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
                        cron_store: cron_store.clone(),
                        sender_handle: self.handle.clone(),
                        flow_store: flow_store.clone(),
                        llm_client: Some(client),
                        mcp_clients: mcp_clients.clone(),
                        skill_store: skill_store.clone(),
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

    /// Build effective system prompt including context summary and handoff context.
    fn effective_system_prompt(&self) -> String {
        let mut prompt = self.system_prompt.clone();

        if let Some(ref handoff) = self.handoff_context {
            prompt.push_str(&format!(
                "\n\n[이전 에이전트에서 인수인계 받은 맥락]\n{}",
                handoff
            ));
        }

        if let Some(ref summary) = self.context_summary {
            prompt.push_str(&format!(
                "\n\n[이전 대화 요약]\n{}",
                summary
            ));
        }

        prompt
    }

    /// Set handoff context from a previous agent's conversation.
    /// This is injected into the system prompt so the receiving agent
    /// has full awareness of prior context.
    pub fn set_handoff_context(&mut self, context: String) {
        self.handoff_context = Some(context);
    }

    /// Summarize this session's conversation for handoff to another agent.
    /// Returns a concise summary suitable for injection into the receiving
    /// agent's system prompt.
    pub async fn summarize_for_handoff(
        &self,
        client: &dyn LlmClient,
    ) -> String {
        // Collect recent conversation text (up to last 20 messages).
        let recent: Vec<&ChatMessage> = self.history.iter().rev().take(20).collect();
        let mut text = String::new();

        // Include existing context summary if available.
        if let Some(ref summary) = self.context_summary {
            text.push_str(&format!("[이전 요약] {summary}\n\n"));
        }

        for msg in recent.iter().rev() {
            let role = &msg.role;
            let content = match &msg.content {
                MessageContent::Text(s) => s.clone(),
                MessageContent::Blocks(blocks) => {
                    blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                }
            };
            if !content.is_empty() {
                text.push_str(&format!("[{role}] {content}\n"));
            }
        }

        if text.is_empty() {
            return String::new();
        }

        // Ask Claude to produce a handoff-optimized summary.
        let summarize_prompt = concat!(
            "다음은 이전 에이전트와 사용자 간의 대화이다. ",
            "새로운 에이전트가 이어받을 수 있도록 핵심 맥락을 정리해줘:\n",
            "1. 사용자가 원래 요청한 것\n",
            "2. 지금까지 완료된 것\n",
            "3. 아직 남은 작업\n",
            "4. 중요한 결정사항이나 제약조건\n\n",
            "5-8문장으로 간결하게. 요약만 출력해."
        );

        let messages = vec![
            ChatMessage::text("user", &format!("{summarize_prompt}\n\n---\n{text}")),
        ];

        match client
            .chat(
                "You are a concise summarizer for agent handoffs.",
                &messages,
                None,
            )
            .await
        {
            Ok(resp) => {
                let summary = resp.text.trim().to_string();
                info!(handle = %self.handle, len = summary.len(), "handoff summary generated");
                summary
            }
            Err(e) => {
                warn!(error = %e, "failed to generate handoff summary, using fallback");
                // Fallback: use the last few user messages as context.
                let mut fallback = String::new();
                for msg in self.history.iter().rev().take(5).rev() {
                    if msg.role == "user" {
                        if let MessageContent::Text(t) = &msg.content {
                            fallback.push_str(&format!("- {t}\n"));
                        }
                    }
                }
                fallback
            }
        }
    }

    /// Summarize dropped messages into the rolling context_summary.
    async fn compact(&mut self, client: &dyn LlmClient, dropped: &[ChatMessage]) {
        // Format dropped messages as text.
        let mut text = String::new();
        for msg in dropped {
            let role = &msg.role;
            let content = match &msg.content {
                MessageContent::Text(s) => s.clone(),
                MessageContent::Blocks(blocks) => {
                    blocks.iter().filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    }).collect::<Vec<_>>().join(" ")
                }
            };
            if !content.is_empty() {
                text.push_str(&format!("[{role}] {content}\n"));
            }
        }

        if text.is_empty() {
            return;
        }

        // Build summarization request (no tools, short).
        let summarize_prompt = "다음 대화를 3-5문장으로 요약해. 핵심 사실, 결정사항, 맥락만 간결하게. 요약만 출력하고 다른 말은 하지 마.";
        let messages = vec![
            ChatMessage::text("user", &format!("{summarize_prompt}\n\n---\n{text}")),
        ];

        match client.chat(
            "You are a concise summarizer. Output only the summary.",
            &messages,
            None,
        ).await {
            Ok(resp) => {
                let new_summary = resp.text.trim().to_string();
                if !new_summary.is_empty() {
                    // Merge with existing summary (keep last 500 chars of old + new).
                    self.context_summary = Some(match &self.context_summary {
                        Some(existing) => {
                            let combined = format!("{existing}\n{new_summary}");
                            // Cap at ~800 chars to avoid bloating the system prompt.
                            if combined.chars().count() > 800 {
                                combined.chars().skip(combined.chars().count() - 800).collect()
                            } else {
                                combined
                            }
                        }
                        None => new_summary,
                    });
                    info!(handle = %self.handle, "context compaction done");
                }
            }
            Err(e) => {
                warn!(error = %e, "context compaction failed, continuing without summary");
            }
        }
    }

    /// Remove oldest messages to stay within `max_history`.
    /// Returns the dropped messages for potential compaction.
    ///
    /// After trimming, ensures the history doesn't start with orphaned
    /// tool_result or assistant tool_use messages (which would cause Claude
    /// API errors due to missing tool_use/tool_result pairs).
    fn trim_history(&mut self) -> Vec<ChatMessage> {
        let mut dropped = Vec::new();

        if self.history.len() > self.max_history {
            let excess = self.history.len() - self.max_history;
            dropped = self.history.drain(..excess).collect();
        }

        // Drop leading messages until we start with a clean user text or
        // assistant text message (not a tool_result or tool_use block).
        while !self.history.is_empty() && self.is_tool_message(0) {
            dropped.push(self.history.remove(0));
        }

        dropped
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

    /// Add extra tool definitions (e.g. from MCP servers) to this session.
    pub fn add_tools(&mut self, extra: Vec<ToolDefinition>) {
        self.tools.extend(extra);
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
