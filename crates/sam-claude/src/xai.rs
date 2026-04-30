//! xAI (Grok) API client — OpenAI-compatible chat completions.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::llm_client::LlmClient;
use crate::types::*;

/// xAI API client (OpenAI-compatible).
pub struct XaiClient {
    client: Client,
    api_key: String,
    model: String,
    max_tokens: u32,
    base_url: String,
    temperature: f32,
    max_retries: u32,
}

impl XaiClient {
    pub fn new(api_key: String, config: &sam_core::LlmConfig) -> anyhow::Result<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
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

// ── OpenAI-compatible wire types ─────────────────────────────────────────

#[derive(Debug, Serialize)]
struct OaiRequest {
    model: String,
    messages: Vec<OaiMessage>,
    max_tokens: u32,
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OaiToolDef>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OaiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OaiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OaiToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: OaiFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OaiFunction {
    name: String,
    arguments: String, // JSON string
}

#[derive(Debug, Serialize)]
struct OaiToolDef {
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
    usage: OaiUsage,
}

#[derive(Debug, Deserialize)]
struct OaiChoice {
    message: OaiChoiceMessage,
    finish_reason: String,
}

#[derive(Debug, Deserialize)]
struct OaiChoiceMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OaiToolCall>>,
}

#[derive(Debug, Deserialize)]
struct OaiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

// ── Conversion helpers ───────────────────────────────────────────────────

/// Convert Sam's ChatMessage to OpenAI message format.
fn to_oai_messages(system: &str, messages: &[ChatMessage]) -> Vec<OaiMessage> {
    let mut oai = Vec::with_capacity(messages.len() + 1);

    // System prompt as first message.
    if !system.is_empty() {
        oai.push(OaiMessage {
            role: "system".to_string(),
            content: Some(serde_json::Value::String(system.to_string())),
            tool_calls: None,
            tool_call_id: None,
        });
    }

    for msg in messages {
        match &msg.content {
            MessageContent::Text(s) => {
                oai.push(OaiMessage {
                    role: msg.role.clone(),
                    content: Some(serde_json::Value::String(s.clone())),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
            MessageContent::Blocks(blocks) => {
                // Assistant message with tool_use blocks.
                let tool_uses: Vec<&ContentBlock> = blocks
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
                    .collect();

                if !tool_uses.is_empty() && msg.role == "assistant" {
                    // Extract text portion if any.
                    let text: Option<String> = blocks.iter().find_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    });

                    let oai_calls: Vec<OaiToolCall> = tool_uses
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolUse { id, name, input } => Some(OaiToolCall {
                                id: id.clone(),
                                call_type: "function".to_string(),
                                function: OaiFunction {
                                    name: name.clone(),
                                    arguments: serde_json::to_string(input)
                                        .unwrap_or_default(),
                                },
                            }),
                            _ => None,
                        })
                        .collect();

                    oai.push(OaiMessage {
                        role: "assistant".to_string(),
                        content: text.map(serde_json::Value::String),
                        tool_calls: Some(oai_calls),
                        tool_call_id: None,
                    });
                }

                // Tool result blocks → role "tool" messages.
                for block in blocks {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = block
                    {
                        oai.push(OaiMessage {
                            role: "tool".to_string(),
                            content: Some(serde_json::Value::String(content.clone())),
                            tool_calls: None,
                            tool_call_id: Some(tool_use_id.clone()),
                        });
                    }
                }
            }
        }
    }

    oai
}

/// Convert Sam's ToolDefinition to OpenAI tool format.
fn to_oai_tools(tools: &[ToolDefinition]) -> Vec<OaiToolDef> {
    tools
        .iter()
        .map(|t| OaiToolDef {
            tool_type: "function".to_string(),
            function: OaiFunctionDef {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.input_schema.clone(),
            },
        })
        .collect()
}

#[async_trait]
impl LlmClient for XaiClient {
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
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            tools: oai_tools,
        };

        let mut retries = 0u32;
        loop {
            let resp = self
                .client
                .post(format!("{}/v1/chat/completions", self.base_url))
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await;

            match resp {
                Ok(response) if response.status().is_success() => {
                    let oai_resp: OaiResponse = response
                        .json()
                        .await
                        .map_err(|e| anyhow::anyhow!("failed to parse xAI response: {e}"))?;

                    let choice = oai_resp
                        .choices
                        .into_iter()
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("xAI response had no choices"))?;

                    let text = choice.message.content.unwrap_or_default();

                    let tool_calls: Vec<ToolCall> = choice
                        .message
                        .tool_calls
                        .unwrap_or_default()
                        .into_iter()
                        .map(|tc| {
                            let input: serde_json::Value =
                                serde_json::from_str(&tc.function.arguments)
                                    .unwrap_or(serde_json::json!({}));
                            ToolCall {
                                id: tc.id,
                                name: tc.function.name,
                                input,
                            }
                        })
                        .collect();

                    let stop_reason = if choice.finish_reason == "tool_calls" {
                        "tool_use".to_string()
                    } else {
                        "end_turn".to_string()
                    };

                    info!(
                        input_tokens = oai_resp.usage.prompt_tokens,
                        output_tokens = oai_resp.usage.completion_tokens,
                        stop_reason = %stop_reason,
                        "xAI API call succeeded"
                    );

                    return Ok(ChatResponse {
                        text,
                        tool_calls,
                        input_tokens: oai_resp.usage.prompt_tokens,
                        output_tokens: oai_resp.usage.completion_tokens,
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
                        return Err(anyhow::anyhow!(
                            "xAI API error: exhausted retries — last status {status}: {body_text}"
                        ));
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
                    return Err(anyhow::anyhow!("xAI API error: HTTP {status}: {body_text}"));
                }
                Err(e) if retries < self.max_retries => {
                    retries += 1;
                    let backoff = Duration::from_secs(1u64 << retries.min(6));
                    warn!(retry = retries, error = %e, "network error, backing off {backoff:?}");
                    tokio::time::sleep(backoff).await;
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("xAI network error: {e}"));
                }
            }
        }
    }
}
