//! Claude Messages API client for Sam.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use tracing::{info, warn};

use crate::llm_client::LlmClient;
use crate::types::*;

/// Lightweight Claude API client tailored for Sam.
pub struct SamClaudeClient {
    client: Client,
    api_key: String,
    model: String,
    max_tokens: u32,
    base_url: String,
    temperature: f32,
    max_retries: u32,
}

impl SamClaudeClient {
    /// Create a new client from an API key and LLM config.
    ///
    /// Returns an error if the HTTP client cannot be constructed (e.g. TLS
    /// backend unavailable).
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

    /// Internal chat implementation shared between direct calls and trait impl.
    async fn chat_impl(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> anyhow::Result<ChatResponse> {
        let api_messages: Vec<ApiMessage> = messages
            .iter()
            .map(|m| {
                let content = match &m.content {
                    MessageContent::Text(s) => serde_json::Value::String(s.clone()),
                    MessageContent::Blocks(blocks) => {
                        serde_json::to_value(blocks)
                            .map_err(|e| anyhow::anyhow!("failed to serialize content blocks: {e}"))?
                    }
                };
                Ok(ApiMessage {
                    role: m.role.clone(),
                    content,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let body = ClaudeApiRequest {
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            system: if system.is_empty() {
                None
            } else {
                Some(system.to_string())
            },
            messages: api_messages,
            temperature: Some(self.temperature),
            tools: tools.map(|t| t.to_vec()),
        };

        let mut retries = 0u32;
        loop {
            let resp = self
                .client
                .post(format!("{}/v1/messages", self.base_url))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await;

            match resp {
                Ok(response) if response.status().is_success() => {
                    let api_resp: ClaudeApiResponse = response
                        .json()
                        .await
                        .map_err(|e| anyhow::anyhow!("failed to parse Claude response: {e}"))?;

                    info!(
                        input_tokens = api_resp.usage.input_tokens,
                        output_tokens = api_resp.usage.output_tokens,
                        stop_reason = %api_resp.stop_reason,
                        "Claude API call succeeded"
                    );

                    // Extract text and tool_use blocks from response.
                    let mut text = String::new();
                    let mut tool_calls = Vec::new();
                    for block in &api_resp.content {
                        match block {
                            ContentBlock::Text { text: t } => {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(t);
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                tool_calls.push(ToolCall {
                                    id: id.clone(),
                                    name: name.clone(),
                                    input: input.clone(),
                                });
                            }
                            ContentBlock::ToolResult { .. } => {}
                            ContentBlock::Image { .. } => {}
                        }
                    }

                    return Ok(ChatResponse {
                        text,
                        tool_calls,
                        input_tokens: api_resp.usage.input_tokens,
                        output_tokens: api_resp.usage.output_tokens,
                        stop_reason: api_resp.stop_reason,
                    });
                }
                Ok(response)
                    if response.status().as_u16() == 429
                        || response.status().is_server_error() =>
                {
                    retries += 1;
                    if retries > self.max_retries {
                        let status = response.status();
                        let body_text = response.text().await
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
                    let body_text = response.text().await
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
                    return Err(sam_core::SamError::ClaudeApi(format!(
                        "network error: {e}"
                    ))
                    .into());
                }
            }
        }
    }
}

#[async_trait]
impl LlmClient for SamClaudeClient {
    async fn chat(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> anyhow::Result<ChatResponse> {
        self.chat_impl(system, messages, tools).await
    }
}
