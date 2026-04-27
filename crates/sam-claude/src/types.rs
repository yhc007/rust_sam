//! Claude Messages API types for Sam — supports text and tool_use.

use serde::{Deserialize, Serialize};

// ── Public types ────────────────────────────────────────────────────────

/// Content of a chat message: plain text or structured content blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// A chat message exchanged between user and assistant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: MessageContent,
}

impl ChatMessage {
    /// Convenience constructor for a simple text message.
    pub fn text(role: &str, content: &str) -> Self {
        Self {
            role: role.to_string(),
            content: MessageContent::Text(content.to_string()),
        }
    }

    /// Build a user message with text and optional inline images.
    /// `images` is a slice of (mime_type, base64_data) pairs.
    pub fn user_with_images(text: &str, images: &[(String, String)]) -> Self {
        let mut blocks = Vec::new();
        // Images first so Claude sees them before the text prompt.
        for (mime_type, data) in images {
            blocks.push(ContentBlock::Image {
                source: ImageSource {
                    source_type: "base64".to_string(),
                    media_type: mime_type.clone(),
                    data: data.clone(),
                },
            });
        }
        if !text.is_empty() {
            blocks.push(ContentBlock::Text {
                text: text.to_string(),
            });
        }
        Self {
            role: "user".to_string(),
            content: MessageContent::Blocks(blocks),
        }
    }

    /// Build a tool_result message (role = "user").
    pub fn tool_result(tool_use_id: &str, result: &str, is_error: bool) -> Self {
        Self {
            role: "user".to_string(),
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: result.to_string(),
                is_error,
            }]),
        }
    }
}

/// A single tool call extracted from a Claude response.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// Parsed response returned to callers after a Claude API round-trip.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub stop_reason: String,
}

/// A tool definition sent to the Claude API.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

// ── Wire types ──────────────────────────────────────────────────────────

/// Request body sent to the Claude Messages API.
#[derive(Debug, Serialize)]
pub(crate) struct ClaudeApiRequest {
    pub model: String,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
}

/// A single message in the API request/response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiMessage {
    pub role: String,
    pub content: serde_json::Value, // String or Vec<ContentBlock>
}

/// Top-level response from the Claude Messages API.
#[derive(Debug, Deserialize)]
pub(crate) struct ClaudeApiResponse {
    #[allow(dead_code)]
    pub id: String,
    pub content: Vec<ContentBlock>,
    #[allow(dead_code)]
    pub model: String,
    pub stop_reason: String,
    pub usage: ApiUsage,
}

/// Source data for an inline image sent to the Claude Vision API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSource {
    /// Always "base64".
    #[serde(rename = "type")]
    pub source_type: String,
    /// MIME type, e.g. "image/jpeg".
    pub media_type: String,
    /// Base64-encoded image data.
    pub data: String,
}

/// Content block in a message — text, image, tool_use, or tool_result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: ImageSource },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

/// Token usage counters.
#[derive(Debug, Deserialize)]
pub(crate) struct ApiUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_text_response() {
        let json = r#"{
            "id": "msg_abc123",
            "type": "message",
            "role": "assistant",
            "content": [
                { "type": "text", "text": "안녕하세요!" }
            ],
            "model": "claude-sonnet-4-20250514",
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": { "input_tokens": 42, "output_tokens": 10 }
        }"#;

        let resp: ClaudeApiResponse = serde_json::from_str(json).expect("deserialize");
        assert_eq!(resp.stop_reason, "end_turn");
        match &resp.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "안녕하세요!"),
            _ => panic!("expected text block"),
        }
    }

    #[test]
    fn chat_message_tool_result_roundtrip() {
        let msg = ChatMessage::tool_result("toolu_01", "2024-01-01 12:00:00", false);
        assert_eq!(msg.role, "user");
        match &msg.content {
            MessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                match &blocks[0] {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        assert_eq!(tool_use_id, "toolu_01");
                        assert_eq!(content, "2024-01-01 12:00:00");
                        assert!(!is_error);
                    }
                    _ => panic!("expected tool_result"),
                }
            }
            _ => panic!("expected blocks"),
        }
    }

    #[test]
    fn tool_result_error_flag() {
        let msg = ChatMessage::tool_result("toolu_02", "unknown tool: foo", true);
        match &msg.content {
            MessageContent::Blocks(blocks) => match &blocks[0] {
                ContentBlock::ToolResult { is_error, .. } => assert!(is_error),
                _ => panic!("expected tool_result"),
            },
            _ => panic!("expected blocks"),
        }
    }

    #[test]
    fn content_block_serialization() {
        let blocks = vec![
            ContentBlock::Text {
                text: "hello".to_string(),
            },
            ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: "current_time".to_string(),
                input: serde_json::json!({}),
            },
        ];
        let json = serde_json::to_value(&blocks).unwrap();
        assert!(json.is_array());
        assert_eq!(json.as_array().unwrap().len(), 2);
        assert_eq!(json[0]["type"], "text");
        assert_eq!(json[1]["type"], "tool_use");
    }

    #[test]
    fn deserialize_tool_use_response() {
        let json = r#"{
            "id": "msg_xyz",
            "type": "message",
            "role": "assistant",
            "content": [
                { "type": "text", "text": "시간을 확인해볼게요." },
                { "type": "tool_use", "id": "toolu_01", "name": "current_time", "input": {} }
            ],
            "model": "claude-sonnet-4-20250514",
            "stop_reason": "tool_use",
            "stop_sequence": null,
            "usage": { "input_tokens": 100, "output_tokens": 50 }
        }"#;

        let resp: ClaudeApiResponse = serde_json::from_str(json).expect("deserialize");
        assert_eq!(resp.stop_reason, "tool_use");
        assert_eq!(resp.content.len(), 2);
        match &resp.content[1] {
            ContentBlock::ToolUse { id, name, .. } => {
                assert_eq!(id, "toolu_01");
                assert_eq!(name, "current_time");
            }
            _ => panic!("expected tool_use block"),
        }
    }
}
