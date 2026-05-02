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
use crate::tools::{builtin_tool_definitions, execute_builtin, ToolContext, MAX_TOOL_ROUNDS, MAX_API_TOOLS};
use crate::types::*;

/// Default maximum characters for the rolling context summary.
const DEFAULT_MAX_SUMMARY_CHARS: usize = 1200;

/// Default max history tokens before compaction kicks in.
const DEFAULT_MAX_HISTORY_TOKENS: usize = 16_000;

/// Truncate a string at a word boundary, appending "..." if truncated.
fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().take(max).collect();
    let mut end = chars.len();
    while end > 0 && !chars[end - 1].is_whitespace() {
        end -= 1;
    }
    if end == 0 {
        end = max;
    }
    format!("{}...", chars[..end].iter().collect::<String>().trim_end())
}

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
    /// Max estimated tokens for history before token-based trim.
    max_context_tokens: usize,
    /// Max chars for the rolling context summary.
    max_summary_chars: usize,
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
            max_context_tokens: DEFAULT_MAX_HISTORY_TOKENS,
            max_summary_chars: DEFAULT_MAX_SUMMARY_CHARS,
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
            max_context_tokens: DEFAULT_MAX_HISTORY_TOKENS,
            max_summary_chars: DEFAULT_MAX_SUMMARY_CHARS,
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
                            max_context_tokens: DEFAULT_MAX_HISTORY_TOKENS,
                            max_summary_chars: DEFAULT_MAX_SUMMARY_CHARS,
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
        let mut effective_system = self.effective_system_prompt();

        // Move memory into mut binding early so we can use it for auto-recall.
        let mut mem_opt = memory;

        // Auto-recall: always search memory before LLM call and inject relevant context.
        if let Some(ref mut mem) = mem_opt {
            let recall_context = mem.recall_context(user_text, 5);
            if !recall_context.is_empty() {
                info!(
                    query = user_text,
                    hits_len = recall_context.len(),
                    "auto-recall injected memory context"
                );
                effective_system.push_str(&format!(
                    "\n\n[관련 기억]\n{}",
                    recall_context
                ));
            }
        }

        // Limit tools sent to the API to avoid overwhelming smaller models.
        // All tools remain executable — only the API request is trimmed.
        let api_tools: Vec<ToolDefinition> = if self.tools.len() > MAX_API_TOOLS {
            // Prioritize core tools, then fill with custom skills up to limit.
            let core_names: std::collections::HashSet<&str> =
                crate::tools::CORE_TOOL_NAMES.iter().copied().collect();
            let mut selected: Vec<ToolDefinition> = self
                .tools
                .iter()
                .filter(|t| core_names.contains(t.name.as_str()))
                .cloned()
                .collect();
            for t in &self.tools {
                if selected.len() >= MAX_API_TOOLS {
                    break;
                }
                if !core_names.contains(t.name.as_str()) {
                    selected.push(t.clone());
                }
            }
            selected
        } else {
            self.tools.clone()
        };
        let tools_slice = &api_tools;
        info!(
            total_tools = self.tools.len(),
            api_tools = api_tools.len(),
            names = ?api_tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            "tool limiting applied"
        );
        let mut total_input = 0u32;
        let mut total_output = 0u32;
        // Track how many times each tool has been called to prevent loops.
        let mut tool_call_counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        const MAX_SAME_TOOL_CALLS: u32 = 2;
        // Collect __ATTACHMENT__ markers from tool results so they're
        // always included in the final response (LLM may omit them).
        let mut collected_attachments: Vec<String> = Vec::new();

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
                    let count = tool_call_counts.entry(tc.name.clone()).or_insert(0);
                    *count += 1;

                    // Block repeated calls to the same tool (prevents loops).
                    if *count > MAX_SAME_TOOL_CALLS {
                        warn!(
                            tool = %tc.name,
                            count = *count,
                            "blocking repeated tool call"
                        );
                        self.history.push(ChatMessage::tool_result(
                            &tc.id,
                            &format!("이 도구({})는 이미 {}회 호출되었다. 더 이상 호출하지 말고 지금까지 결과를 사용자에게 전달해라.", tc.name, *count - 1),
                            true,
                        ));
                        continue;
                    }

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
                    // Collect __ATTACHMENT__ markers from tool results.
                    if !is_error {
                        for line in result_text.lines() {
                            if let Some(path) = line.strip_prefix("__ATTACHMENT__:") {
                                collected_attachments.push(path.to_string());
                            }
                        }
                    }
                    info!(
                        tool = %tc.name,
                        is_error = is_error,
                        count = *count,
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

            // Handle empty response from LLM.
            let mut final_text = if resp.text.trim().is_empty() {
                warn!(handle = %self.handle, "LLM returned empty response");
                "미안, 응답을 생성하지 못했어. 다시 말해줘.".to_string()
            } else {
                resp.text
            };

            // Append collected __ATTACHMENT__ markers that the LLM may
            // have omitted from its final response.
            for att in &collected_attachments {
                if !final_text.contains(&format!("__ATTACHMENT__:{att}")) {
                    final_text.push_str(&format!("\n__ATTACHMENT__:{att}"));
                }
            }

            // Append final assistant message.
            self.history.push(ChatMessage::text("assistant", &final_text));
            self.trim_history();

            // Auto-store conversation in memory.
            if let Some(mem) = mem_opt.as_deref_mut() {
                if !final_text.starts_with("미안, 응답을") {
                    if let Err(e) = mem.store_conversation(&self.handle, user_text, &final_text) {
                        warn!(error = %e, "failed to store conversation memory");
                    }
                }
            }

            return Ok(final_text);
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
    ///
    /// Uses structured summarization that preserves:
    /// - Key facts and decisions
    /// - Tool actions and their outcomes
    /// - User preferences expressed during conversation
    /// - Ongoing tasks or pending items
    async fn compact(&mut self, client: &dyn LlmClient, dropped: &[ChatMessage]) {
        let text = Self::format_messages_for_summary(dropped);
        if text.is_empty() {
            return;
        }

        // If existing summary is already large, do a two-phase compaction:
        // first re-summarize the old summary + new dropped into a fresh one.
        let needs_recompact = self
            .context_summary
            .as_ref()
            .is_some_and(|s| s.chars().count() > 600);

        let summarize_prompt = if needs_recompact {
            // Re-summarize everything into a structured format.
            format!(
                concat!(
                    "기존 대화 요약과 새로운 대화 내역이 있다. ",
                    "전체를 하나의 구조화된 요약으로 통합해줘.\n\n",
                    "반드시 다음 형식으로 출력:\n",
                    "## 핵심 사실\n- (중요한 사실/결정 bullet points)\n\n",
                    "## 도구 사용 이력\n- (어떤 도구로 무엇을 했는지)\n\n",
                    "## 진행 중인 작업\n- (아직 완료되지 않은 것)\n\n",
                    "## 사용자 선호\n- (대화 중 드러난 선호/제약)\n\n",
                    "각 섹션은 해당 내용이 있을 때만 포함. 최대 10줄.\n\n",
                    "---\n[기존 요약]\n{}\n\n[새 대화]\n{}"
                ),
                self.context_summary.as_deref().unwrap_or(""),
                text
            )
        } else {
            format!(
                concat!(
                    "다음 대화를 구조화된 요약으로 만들어줘.\n\n",
                    "반드시 다음 형식으로 출력:\n",
                    "## 핵심 사실\n- (중요한 사실/결정 bullet points)\n\n",
                    "## 도구 사용 이력\n- (어떤 도구로 무엇을 했는지, 없으면 생략)\n\n",
                    "## 진행 중인 작업\n- (아직 완료되지 않은 것, 없으면 생략)\n\n",
                    "## 사용자 선호\n- (대화 중 드러난 선호/제약, 없으면 생략)\n\n",
                    "각 섹션은 해당 내용이 있을 때만 포함. 최대 8줄. 요약만 출력.\n\n---\n{}"
                ),
                text
            )
        };

        let messages = vec![ChatMessage::text("user", &summarize_prompt)];

        match client
            .chat(
                "You are a structured conversation summarizer. Output only the structured summary in the requested format.",
                &messages,
                None,
            )
            .await
        {
            Ok(resp) => {
                let new_summary = resp.text.trim().to_string();
                if !new_summary.is_empty() {
                    if needs_recompact {
                        // Full re-summarization: replace entirely.
                        self.context_summary = Some(Self::cap_summary(&new_summary, self.max_summary_chars));
                    } else {
                        // Append mode: merge with existing.
                        self.context_summary = Some(match &self.context_summary {
                            Some(existing) => {
                                let combined = format!("{existing}\n\n{new_summary}");
                                Self::cap_summary(&combined, self.max_summary_chars)
                            }
                            None => new_summary,
                        });
                    }
                    info!(
                        handle = %self.handle,
                        summary_len = self.context_summary.as_ref().map(|s| s.len()).unwrap_or(0),
                        recompacted = needs_recompact,
                        "context compaction done"
                    );
                }
            }
            Err(e) => {
                warn!(error = %e, "context compaction failed, using extractive fallback");
                // Extractive fallback: keep key sentences from dropped messages.
                let fallback = Self::extractive_fallback(dropped);
                if !fallback.is_empty() {
                    self.context_summary = Some(match &self.context_summary {
                        Some(existing) => {
                            Self::cap_summary(&format!("{existing}\n{fallback}"), self.max_summary_chars)
                        }
                        None => fallback,
                    });
                }
            }
        }
    }

    /// Format messages for summarization, including tool use context.
    fn format_messages_for_summary(messages: &[ChatMessage]) -> String {
        let mut text = String::new();
        for msg in messages {
            let role = &msg.role;
            match &msg.content {
                MessageContent::Text(s) => {
                    if !s.is_empty() {
                        text.push_str(&format!("[{role}] {s}\n"));
                    }
                }
                MessageContent::Blocks(blocks) => {
                    for block in blocks {
                        match block {
                            ContentBlock::Text { text: t } => {
                                if !t.is_empty() {
                                    text.push_str(&format!("[{role}] {t}\n"));
                                }
                            }
                            ContentBlock::ToolUse { name, input, .. } => {
                                // Include tool calls with abbreviated input.
                                let input_str = input.to_string();
                                let abbreviated = truncate_chars(&input_str, 100);
                                text.push_str(&format!("[tool:{name}] {abbreviated}\n"));
                            }
                            ContentBlock::ToolResult {
                                content, is_error, ..
                            } => {
                                let prefix = if *is_error { "error" } else { "result" };
                                let abbreviated = truncate_chars(content, 150);
                                text.push_str(&format!("[{prefix}] {abbreviated}\n"));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        text
    }

    /// Cap summary at a character limit, cutting at the last complete line.
    fn cap_summary(text: &str, max_chars: usize) -> String {
        if text.chars().count() <= max_chars {
            return text.to_string();
        }
        // Find the last newline before the limit.
        let chars: Vec<char> = text.chars().collect();
        let mut cut_at = max_chars;
        while cut_at > 0 && chars[cut_at] != '\n' {
            cut_at -= 1;
        }
        if cut_at == 0 {
            // No newline found; cut at limit.
            chars[..max_chars].iter().collect()
        } else {
            chars[..cut_at].iter().collect()
        }
    }

    /// Extractive fallback when LLM summarization fails.
    /// Pulls out user messages and short assistant responses.
    fn extractive_fallback(messages: &[ChatMessage]) -> String {
        let mut lines = Vec::new();
        for msg in messages {
            if let MessageContent::Text(s) = &msg.content {
                if msg.role == "user" && !s.is_empty() {
                    lines.push(format!("- [user] {}", truncate_str(s, 80)));
                } else if msg.role == "assistant" && s.len() < 100 && !s.is_empty() {
                    lines.push(format!("- [assistant] {s}"));
                }
            }
        }
        // Keep last 6 lines.
        let start = lines.len().saturating_sub(6);
        lines[start..].join("\n")
    }

    /// Remove oldest messages to stay within `max_history`.
    /// Returns the dropped messages for potential compaction.
    ///
    /// Uses a hybrid strategy:
    /// 1. Message count limit (max_history)
    /// 2. Estimated token limit (~16K tokens of history)
    ///
    /// After trimming, ensures the history doesn't start with orphaned
    /// tool_result or assistant tool_use messages (which would cause Claude
    /// API errors due to missing tool_use/tool_result pairs).
    fn trim_history(&mut self) -> Vec<ChatMessage> {
        let mut dropped = Vec::new();

        // Phase 1: Count-based trim.
        if self.history.len() > self.max_history {
            let excess = self.history.len() - self.max_history;
            dropped = self.history.drain(..excess).collect();
        }

        // Phase 2: Token-based trim — estimate and drop if over budget.
        // Rough estimate: 1 token ≈ 3.5 chars for mixed Korean/English.
        const CHARS_PER_TOKEN: f32 = 3.5;
        let total_chars: usize = self.history.iter().map(Self::estimate_message_chars).sum();
        let estimated_tokens = (total_chars as f32 / CHARS_PER_TOKEN) as usize;

        if estimated_tokens > self.max_context_tokens {
            // Drop oldest messages until we're under budget.
            let target_chars = (self.max_context_tokens as f32 * CHARS_PER_TOKEN) as usize;
            let mut current_chars = total_chars;
            while current_chars > target_chars && !self.history.is_empty() {
                let msg = self.history.remove(0);
                current_chars -= Self::estimate_message_chars(&msg);
                dropped.push(msg);
            }
        }

        // Phase 3: Drop leading messages until we start with a clean user text or
        // assistant text message (not a tool_result or tool_use block).
        while !self.history.is_empty() && self.is_tool_message(0) {
            dropped.push(self.history.remove(0));
        }

        dropped
    }

    /// Estimate character count of a message (for token budget estimation).
    fn estimate_message_chars(msg: &ChatMessage) -> usize {
        match &msg.content {
            MessageContent::Text(s) => s.len(),
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => text.len(),
                    ContentBlock::ToolUse { input, .. } => input.to_string().len(),
                    ContentBlock::ToolResult { content, .. } => content.len(),
                    _ => 0,
                })
                .sum(),
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

    /// Number of tool definitions registered in this session.
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    /// Add extra tool definitions (e.g. from MCP servers) to this session.
    pub fn add_tools(&mut self, extra: Vec<ToolDefinition>) {
        self.tools.extend(extra);
    }

    /// Configure compaction limits from SamConfig.
    pub fn set_compaction_limits(&mut self, max_context_tokens: usize, max_summary_chars: usize) {
        self.max_context_tokens = max_context_tokens;
        self.max_summary_chars = max_summary_chars;
    }
}

/// Truncate a string to at most `max_chars` characters, appending "..." if truncated.
/// Safe for multi-byte characters (Korean, etc.).
fn truncate_chars(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{truncated}...")
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

    #[test]
    fn token_based_trim_drops_large_messages() {
        let mut session = ConversationSession::new(
            "+821012345678",
            "system prompt".to_string(),
            100, // high message count limit — won't trigger count-based trim
        );
        // Set low token limit to trigger token-based trim.
        session.max_context_tokens = 100; // ~350 chars budget

        // Add messages totaling much more than 350 chars.
        session.history.push(ChatMessage::text("user", &"a".repeat(200)));
        session.history.push(ChatMessage::text("assistant", &"b".repeat(200)));
        session.history.push(ChatMessage::text("user", &"c".repeat(200)));

        let dropped = session.trim_history();
        assert!(!dropped.is_empty());
        // Should have some messages remaining.
        assert!(!session.history.is_empty());
    }

    #[test]
    fn cap_summary_respects_line_boundary() {
        let text = "line1\nline2\nline3\nline4\nline5";
        let capped = ConversationSession::cap_summary(text, 18);
        // Should cut at a newline boundary.
        assert!(capped.ends_with("line2") || capped.ends_with("line3"));
        assert!(capped.len() <= 18);
    }

    #[test]
    fn format_messages_includes_tool_use() {
        let messages = vec![
            ChatMessage::text("user", "시간 알려줘"),
            ChatMessage {
                role: "assistant".to_string(),
                content: MessageContent::Blocks(vec![
                    ContentBlock::ToolUse {
                        id: "t1".to_string(),
                        name: "current_time".to_string(),
                        input: serde_json::json!({}),
                    },
                ]),
            },
            ChatMessage::tool_result("t1", "2026-04-28 10:00", false),
        ];

        let formatted = ConversationSession::format_messages_for_summary(&messages);
        assert!(formatted.contains("[user] 시간 알려줘"));
        assert!(formatted.contains("[tool:current_time]"));
        assert!(formatted.contains("[result] 2026-04-28 10:00"));
    }

    #[test]
    fn extractive_fallback_keeps_recent() {
        let messages: Vec<ChatMessage> = (0..10)
            .map(|i| ChatMessage::text("user", &format!("message {i}")))
            .collect();

        let fallback = ConversationSession::extractive_fallback(&messages);
        // Should keep last 6.
        assert!(fallback.contains("message 4"));
        assert!(fallback.contains("message 9"));
        assert!(!fallback.contains("message 3"));
    }

    #[test]
    fn truncate_str_at_word_boundary() {
        let s = "hello world this is a test";
        let truncated = truncate_str(s, 12);
        // "hello world " is 12 chars, truncates at that word boundary
        assert_eq!(truncated, "hello world...");

        let truncated2 = truncate_str(s, 8);
        // Cuts at space before position 8
        assert_eq!(truncated2, "hello...");
    }

    #[test]
    fn truncate_str_short_string_unchanged() {
        let s = "short";
        assert_eq!(truncate_str(s, 100), "short");
    }

    #[test]
    fn effective_prompt_includes_handoff_and_summary() {
        let mut session = ConversationSession::new(
            "test",
            "base prompt".to_string(),
            20,
        );
        session.handoff_context = Some("handoff info".to_string());
        session.context_summary = Some("summary info".to_string());

        let effective = session.effective_system_prompt();
        assert!(effective.contains("base prompt"));
        assert!(effective.contains("handoff info"));
        assert!(effective.contains("summary info"));
        // Handoff should come before summary.
        let handoff_pos = effective.find("handoff info").unwrap();
        let summary_pos = effective.find("summary info").unwrap();
        assert!(handoff_pos < summary_pos);
    }
}
