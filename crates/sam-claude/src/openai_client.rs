//! OpenAI-compatible API client for Sam.
//!
//! Works with any provider that implements the OpenAI Chat Completions API:
//! vLLM (local Nemotron), xAI Grok, etc.

use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::backend::LlmBackend;
use crate::types::*;

/// OpenAI-compatible API client.
pub struct OpenAiCompatibleClient {
    client: Client,
    api_key: String,
    model: String,
    max_tokens: u32,
    base_url: String,
    temperature: f32,
    max_retries: u32,
}

impl OpenAiCompatibleClient {
    /// Create a new client from an API key and LLM config.
    pub fn new(api_key: String, config: &sam_core::LlmConfig) -> anyhow::Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build HTTP client: {e}"))?;

        Ok(Self {
            client,
            api_key,
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            base_url: config.base_url.clone(),
            temperature: config.temperature,
            max_retries: config.max_retries,
        })
    }
}

// ── OpenAI wire types (private) ────────────────────────────────────────

#[derive(Debug, Serialize)]
struct OaiRequest {
    model: String,
    messages: Vec<OaiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OaiTool>>,
}

#[derive(Debug, Serialize)]
struct OaiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OaiToolCallOut>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct OaiToolCallOut {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: OaiFunctionCallOut,
}

#[derive(Debug, Serialize)]
struct OaiFunctionCallOut {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct OaiTool {
    #[serde(rename = "type")]
    tool_type: String,
    function: OaiFunctionDef,
}

#[derive(Debug, Serialize)]
struct OaiFunctionDef {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct OaiResponse {
    choices: Vec<OaiChoice>,
    #[serde(default)]
    usage: Option<OaiUsage>,
}

#[derive(Debug, Deserialize)]
struct OaiChoice {
    message: OaiChoiceMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OaiChoiceMessage {
    #[allow(dead_code)]
    role: String,
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OaiToolCallIn>>,
}

#[derive(Debug, Deserialize)]
struct OaiToolCallIn {
    id: String,
    function: OaiFunctionCallIn,
}

#[derive(Debug, Deserialize)]
struct OaiFunctionCallIn {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OaiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

// ── Conversion helpers ─────────────────────────────────────────────────

/// Convert Sam ToolDefinition to OpenAI tool format.
fn to_oai_tools(tools: &[ToolDefinition]) -> Vec<OaiTool> {
    tools
        .iter()
        .map(|t| OaiTool {
            tool_type: "function".to_string(),
            function: OaiFunctionDef {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.input_schema.clone(),
            },
        })
        .collect()
}

/// Convert Sam ChatMessage history to OpenAI message format.
fn to_oai_messages(system: &str, messages: &[ChatMessage]) -> Vec<OaiMessage> {
    let mut out = Vec::with_capacity(messages.len() + 1);

    // System prompt as first message.
    if !system.is_empty() {
        out.push(OaiMessage {
            role: "system".to_string(),
            content: Some(system.to_string()),
            tool_calls: None,
            tool_call_id: None,
        });
    }

    for msg in messages {
        match &msg.content {
            MessageContent::Text(s) => {
                out.push(OaiMessage {
                    role: msg.role.clone(),
                    content: Some(s.clone()),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
            MessageContent::Blocks(blocks) => {
                if msg.role == "assistant" {
                    // Assistant message with tool_use blocks.
                    let mut text_parts = Vec::new();
                    let mut tool_calls = Vec::new();

                    for block in blocks {
                        match block {
                            ContentBlock::Text { text } => {
                                text_parts.push(text.clone());
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                tool_calls.push(OaiToolCallOut {
                                    id: id.clone(),
                                    call_type: "function".to_string(),
                                    function: OaiFunctionCallOut {
                                        name: name.clone(),
                                        arguments: serde_json::to_string(input)
                                            .unwrap_or_default(),
                                    },
                                });
                            }
                            ContentBlock::ToolResult { .. } => {}
                            ContentBlock::Image { .. } => {}
                        }
                    }

                    let content = if text_parts.is_empty() {
                        None
                    } else {
                        Some(text_parts.join("\n"))
                    };

                    out.push(OaiMessage {
                        role: "assistant".to_string(),
                        content,
                        tool_calls: if tool_calls.is_empty() {
                            None
                        } else {
                            Some(tool_calls)
                        },
                        tool_call_id: None,
                    });
                } else {
                    // User message with tool_result blocks — expand to
                    // individual "tool" role messages (OpenAI format).
                    for block in blocks {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } = block
                        {
                            out.push(OaiMessage {
                                role: "tool".to_string(),
                                content: Some(content.clone()),
                                tool_calls: None,
                                tool_call_id: Some(tool_use_id.clone()),
                            });
                        }
                    }
                }
            }
        }
    }

    out
}

// ── LlmBackend implementation ──────────────────────────────────────────

#[async_trait::async_trait]
impl LlmBackend for OpenAiCompatibleClient {
    async fn chat(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> anyhow::Result<ChatResponse> {
        let oai_messages = to_oai_messages(system, messages);
        let oai_tools = tools.map(to_oai_tools);

        let body = OaiRequest {
            model: self.model.clone(),
            messages: oai_messages,
            temperature: Some(self.temperature),
            max_tokens: Some(self.max_tokens),
            tools: oai_tools,
        };

        let mut retries = 0u32;
        loop {
            let mut req = self
                .client
                .post(format!("{}/v1/chat/completions", self.base_url))
                .header("content-type", "application/json");

            // Only add auth header if we have a non-empty key (local vLLM
            // may not need one).
            if !self.api_key.is_empty() {
                req = req.header("authorization", format!("Bearer {}", self.api_key));
            }

            let resp = req.json(&body).send().await;

            match resp {
                Ok(response) if response.status().is_success() => {
                    let oai_resp: OaiResponse = response
                        .json()
                        .await
                        .map_err(|e| anyhow::anyhow!("failed to parse response: {e}"))?;

                    let choice = oai_resp
                        .choices
                        .into_iter()
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("empty choices in response"))?;

                    let text = choice.message.content.unwrap_or_default();

                    let mut tool_calls = Vec::new();
                    if let Some(tcs) = choice.message.tool_calls {
                        for tc in tcs {
                            let input: serde_json::Value =
                                serde_json::from_str(&tc.function.arguments).unwrap_or_else(
                                    |_| serde_json::Value::String(tc.function.arguments.clone()),
                                );
                            tool_calls.push(ToolCall {
                                id: tc.id,
                                name: tc.function.name,
                                input,
                            });
                        }
                    }

                    let usage = oai_resp.usage.unwrap_or(OaiUsage {
                        prompt_tokens: 0,
                        completion_tokens: 0,
                    });

                    // Map OpenAI finish_reason to Anthropic stop_reason so the
                    // session loop works unchanged.
                    let finish = choice.finish_reason.unwrap_or_default();
                    let stop_reason = match finish.as_str() {
                        "tool_calls" => "tool_use".to_string(),
                        "stop" => "end_turn".to_string(),
                        "length" => "max_tokens".to_string(),
                        other => other.to_string(),
                    };

                    info!(
                        input_tokens = usage.prompt_tokens,
                        output_tokens = usage.completion_tokens,
                        stop_reason = %stop_reason,
                        "OpenAI-compatible API call succeeded"
                    );

                    return Ok(ChatResponse {
                        text,
                        tool_calls,
                        input_tokens: usage.prompt_tokens,
                        output_tokens: usage.completion_tokens,
                        stop_reason,
                    });
                }
                Ok(response)
                    if response.status().as_u16() == 429
                        || response.status().is_server_error() =>
                {
                    retries += 1;
                    if retries > self.max_retries {
                        let status = response.status();
                        let body_text = response
                            .text()
                            .await
                            .unwrap_or_else(|e| format!("<body unreadable: {e}>"));
                        return Err(sam_core::SamError::ClaudeApi(format!(
                            "exhausted retries — last status {status}: {body_text}"
                        ))
                        .into());
                    }
                    let backoff = Duration::from_secs(1u64 << retries.min(6));
                    warn!(retry = retries, "retryable error, backing off {backoff:?}");
                    tokio::time::sleep(backoff).await;
                }
                Ok(response) => {
                    let status = response.status();
                    let body_text = response
                        .text()
                        .await
                        .unwrap_or_else(|e| format!("<body unreadable: {e}>"));
                    return Err(sam_core::SamError::ClaudeApi(format!(
                        "HTTP {status}: {body_text}"
                    ))
                    .into());
                }
                Err(e) if retries < self.max_retries => {
                    retries += 1;
                    let backoff = Duration::from_secs(1u64 << retries.min(6));
                    warn!(retry = retries, error = %e, "network error, backing off {backoff:?}");
                    tokio::time::sleep(backoff).await;
                }
                Err(e) => {
                    return Err(
                        sam_core::SamError::ClaudeApi(format!("network error: {e}")).into(),
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definition_maps_to_oai() {
        let tools = vec![ToolDefinition {
            name: "current_time".to_string(),
            description: "Get the current time".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }];

        let oai = to_oai_tools(&tools);
        assert_eq!(oai.len(), 1);
        assert_eq!(oai[0].tool_type, "function");
        assert_eq!(oai[0].function.name, "current_time");
    }

    #[test]
    fn messages_convert_with_system() {
        let messages = vec![
            ChatMessage::text("user", "hello"),
            ChatMessage::text("assistant", "hi"),
        ];

        let oai = to_oai_messages("You are Sam.", &messages);
        assert_eq!(oai.len(), 3);
        assert_eq!(oai[0].role, "system");
        assert_eq!(oai[0].content.as_deref(), Some("You are Sam."));
        assert_eq!(oai[1].role, "user");
        assert_eq!(oai[2].role, "assistant");
    }

    #[test]
    fn tool_result_expands_to_tool_role() {
        let messages = vec![ChatMessage::tool_result("call_1", "result text", false)];

        let oai = to_oai_messages("", &messages);
        assert_eq!(oai.len(), 1);
        assert_eq!(oai[0].role, "tool");
        assert_eq!(oai[0].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(oai[0].content.as_deref(), Some("result text"));
    }

    #[test]
    fn assistant_tool_use_blocks_convert() {
        let msg = ChatMessage {
            role: "assistant".to_string(),
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "Let me check.".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "current_time".to_string(),
                    input: serde_json::json!({}),
                },
            ]),
        };

        let oai = to_oai_messages("", &[msg]);
        assert_eq!(oai.len(), 1);
        assert_eq!(oai[0].role, "assistant");
        assert_eq!(oai[0].content.as_deref(), Some("Let me check."));
        let tcs = oai[0].tool_calls.as_ref().unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].function.name, "current_time");
    }
}
