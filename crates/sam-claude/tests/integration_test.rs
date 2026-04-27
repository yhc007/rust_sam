//! Integration tests for Sam's agentic loop using a mock LLM backend.
//!
//! These tests verify the full conversation flow:
//! - Simple text replies
//! - Tool use → tool_result → final text (multi-round)
//! - Budget enforcement
//! - Session history management

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::json;

use sam_claude::budget::TokenBudget;
use sam_claude::session::ConversationSession;
use sam_claude::types::{ChatMessage, ChatResponse, ToolCall, ToolDefinition};
use sam_claude::LlmBackend;
use sam_core::SamConfig;

// ── Mock LLM Backend ─────────────────────────────────────────────────────

/// A scripted mock LLM that returns predefined responses in sequence.
struct MockLlm {
    responses: Vec<ChatResponse>,
    call_count: AtomicUsize,
}

impl MockLlm {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses,
            call_count: AtomicUsize::new(0),
        }
    }

    /// How many times chat() was called.
    fn calls(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmBackend for MockLlm {
    async fn chat(
        &self,
        _system: &str,
        _messages: &[ChatMessage],
        _tools: Option<&[ToolDefinition]>,
    ) -> anyhow::Result<ChatResponse> {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        if idx < self.responses.len() {
            Ok(self.responses[idx].clone())
        } else {
            // Fallback: return a simple end_turn response.
            Ok(ChatResponse {
                text: "[mock exhausted]".to_string(),
                tool_calls: vec![],
                input_tokens: 10,
                output_tokens: 5,
                stop_reason: "end_turn".to_string(),
            })
        }
    }
}

/// Helper to build a simple text response.
fn text_response(text: &str) -> ChatResponse {
    ChatResponse {
        text: text.to_string(),
        tool_calls: vec![],
        input_tokens: 50,
        output_tokens: 30,
        stop_reason: "end_turn".to_string(),
    }
}

/// Helper to build a tool_use response.
fn tool_use_response(tool_name: &str, input: serde_json::Value) -> ChatResponse {
    ChatResponse {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: format!("toolu_{tool_name}"),
            name: tool_name.to_string(),
            input,
        }],
        input_tokens: 50,
        output_tokens: 40,
        stop_reason: "tool_use".to_string(),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn simple_text_reply() {
    let mock = MockLlm::new(vec![text_response("안녕! 나는 Sam이야.")]);
    let mut session = ConversationSession::new("test_handle", "system prompt".into(), 20);
    let mut budget = TokenBudget::load_or_new(100_000);
    let config = SamConfig::default();

    let reply = session
        .reply(&mock, &mut budget, "안녕", &[], None, &config, None, None, None, None)
        .await
        .unwrap();

    assert_eq!(reply, "안녕! 나는 Sam이야.");
    assert_eq!(mock.calls(), 1);
    // History should have user + assistant messages.
    assert_eq!(session.history().len(), 2);
}

#[tokio::test]
async fn tool_use_single_round() {
    // Mock: first call returns tool_use (current_time), second call returns final text.
    let mock = MockLlm::new(vec![
        tool_use_response("current_time", json!({})),
        text_response("지금은 2026-04-28 00:30:00 (월요일)이야."),
    ]);

    let mut session = ConversationSession::new("test_handle", "system prompt".into(), 20);
    let mut budget = TokenBudget::load_or_new(100_000);
    let config = SamConfig::default();

    let reply = session
        .reply(&mock, &mut budget, "지금 몇 시야?", &[], None, &config, None, None, None, None)
        .await
        .unwrap();

    assert_eq!(reply, "지금은 2026-04-28 00:30:00 (월요일)이야.");
    // Should have called the mock twice (tool_use + end_turn).
    assert_eq!(mock.calls(), 2);
    // History: user, assistant(tool_use), user(tool_result), assistant(final)
    assert_eq!(session.history().len(), 4);
}

#[tokio::test]
async fn tool_use_multi_round() {
    // Simulate: 1st call → tool_use(memory_store), 2nd call → tool_use(current_time), 3rd → text
    let mock = MockLlm::new(vec![
        tool_use_response("current_time", json!({})),
        tool_use_response("current_time", json!({})),
        text_response("두 번 시간을 확인했어. 현재 시간이야."),
    ]);

    let mut session = ConversationSession::new("test_handle", "system prompt".into(), 40);
    let mut budget = TokenBudget::load_or_new(100_000);
    let config = SamConfig::default();

    let reply = session
        .reply(&mock, &mut budget, "시간 두 번 확인해", &[], None, &config, None, None, None, None)
        .await
        .unwrap();

    assert_eq!(reply, "두 번 시간을 확인했어. 현재 시간이야.");
    assert_eq!(mock.calls(), 3);
}

#[tokio::test]
async fn budget_exceeded_returns_limit_message() {
    let mock = MockLlm::new(vec![text_response("Hello!")]);
    let mut session = ConversationSession::new("test_handle", "system prompt".into(), 20);
    // Very small budget — will be exceeded by the response tokens.
    let mut budget = TokenBudget::load_or_new(10);

    let reply = session
        .reply(&mock, &mut budget, "hi", &[], None, &SamConfig::default(), None, None, None, None)
        .await
        .unwrap();

    // Should return the budget-exceeded Korean message.
    assert!(reply.contains("토큰 한도"), "expected budget message, got: {reply}");
}

#[tokio::test]
async fn session_history_trimmed() {
    let mock = MockLlm::new(vec![
        text_response("reply 1"),
        text_response("reply 2"),
        text_response("reply 3"),
        text_response("reply 4"),
    ]);

    // max_history = 4 — only keeps last 4 messages.
    let mut session = ConversationSession::new("test_handle", "system prompt".into(), 4);
    let mut budget = TokenBudget::load_or_new(100_000);
    let config = SamConfig::default();

    for i in 1..=4 {
        session
            .reply(&mock, &mut budget, &format!("msg {i}"), &[], None, &config, None, None, None, None)
            .await
            .unwrap();
    }

    // Should be trimmed to max_history = 4.
    assert!(session.history().len() <= 4);
}

#[tokio::test]
async fn api_error_does_not_grow_history() {
    /// A mock that always fails.
    struct FailingLlm;

    #[async_trait]
    impl LlmBackend for FailingLlm {
        async fn chat(
            &self,
            _system: &str,
            _messages: &[ChatMessage],
            _tools: Option<&[ToolDefinition]>,
        ) -> anyhow::Result<ChatResponse> {
            anyhow::bail!("API is down")
        }
    }

    let mock = FailingLlm;
    let mut session = ConversationSession::new("test_handle", "system prompt".into(), 20);
    let mut budget = TokenBudget::load_or_new(100_000);
    let config = SamConfig::default();

    let result = session
        .reply(&mock, &mut budget, "hello", &[], None, &config, None, None, None, None)
        .await;

    assert!(result.is_err());
    // History should NOT have grown (user message was popped on error).
    assert_eq!(session.history().len(), 0);
}

#[tokio::test]
async fn tool_result_appears_in_history() {
    // Verify that after a tool_use round, the tool_result is properly in history.
    let mock = MockLlm::new(vec![
        tool_use_response("current_time", json!({})),
        text_response("Done."),
    ]);

    let mut session = ConversationSession::new("test_handle", "system prompt".into(), 20);
    let mut budget = TokenBudget::load_or_new(100_000);
    let config = SamConfig::default();

    session
        .reply(&mock, &mut budget, "time?", &[], None, &config, None, None, None, None)
        .await
        .unwrap();

    // History: [user("time?"), assistant(tool_use), user(tool_result), assistant("Done.")]
    let history = session.history();
    assert_eq!(history.len(), 4);
    assert_eq!(history[0].role, "user");
    assert_eq!(history[1].role, "assistant");
    assert_eq!(history[2].role, "user"); // tool_result
    assert_eq!(history[3].role, "assistant");
}

#[tokio::test]
async fn session_with_tool_filter() {
    // Create a session with only current_time allowed.
    let filter = sam_core::ToolFilter::Allow {
        names: vec!["current_time".to_string()],
    };
    let session = ConversationSession::new_with_filter(
        "test_handle",
        "system".into(),
        20,
        &filter,
    );

    // The session should have exactly 1 tool (current_time).
    // We can't directly inspect tools, but we can verify it was created without panic.
    assert_eq!(session.history().len(), 0);
}

#[tokio::test]
async fn context_compaction_on_trim() {
    // Use a mock that also handles the compaction summarization call.
    let mock = MockLlm::new(vec![
        text_response("reply 1"),
        text_response("reply 2"),
        text_response("reply 3"),
        // The 4th call is for compaction (summarization).
        text_response("이전 대화 요약: 사용자가 인사하고 Sam이 답함."),
        text_response("reply 4"),
    ]);

    // max_history = 4, so after 3 messages (6 items), trimming kicks in.
    let mut session = ConversationSession::new("test_handle", "system prompt".into(), 4);
    let mut budget = TokenBudget::load_or_new(100_000);
    let config = SamConfig::default();

    for i in 1..=3 {
        session
            .reply(&mock, &mut budget, &format!("msg {i}"), &[], None, &config, None, None, None, None)
            .await
            .unwrap();
    }

    // After trimming + compaction, history should be within bounds.
    assert!(session.history().len() <= 4);
}
