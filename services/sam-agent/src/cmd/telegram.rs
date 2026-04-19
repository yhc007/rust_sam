//! `sam telegram` — Telegram bot mode.
//!
//! Long-polls the Telegram Bot API for messages, routes them through the
//! configured LLM backend with full tool support, and replies in-chat.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use sam_claude::{
    load_api_key, load_system_prompt, ConversationSession, LlmBackend, OpenAiCompatibleClient,
    SamClaudeClient, TokenBudget,
};
use sam_core::{config_path, load_config};
use sam_memory_adapter::MemoryAdapter;

pub async fn run() -> i32 {
    let config = match load_config(config_path()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            return 2;
        }
    };

    if !config.telegram.enabled {
        eprintln!("error: [telegram] enabled = false in config.toml");
        return 2;
    }

    // Load bot token.
    let bot_token = match load_token(&config.telegram.bot_token_source) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Telegram bot token error: {e}");
            return 2;
        }
    };

    // Load LLM API key.
    let api_key = match load_api_key(&config.llm) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("API key error: {e}");
            return 2;
        }
    };

    let client: Arc<dyn LlmBackend> = match config.llm.provider.as_str() {
        "openai-compatible" => match OpenAiCompatibleClient::new(api_key, &config.llm) {
            Ok(c) => Arc::new(c),
            Err(e) => {
                eprintln!("HTTP client error: {e}");
                return 2;
            }
        },
        _ => match SamClaudeClient::new(api_key, &config.llm) {
            Ok(c) => Arc::new(c),
            Err(e) => {
                eprintln!("HTTP client error: {e}");
                return 2;
            }
        },
    };

    let system_prompt = load_system_prompt();
    let max_history = config.llm.max_history;
    let mut budget = TokenBudget::load_or_new(config.llm.daily_token_budget);

    // Long-term memory (optional).
    let mut memory: Option<MemoryAdapter> = match MemoryAdapter::from_config(&config.memory) {
        Ok(m) => {
            let stats = m.stats();
            info!(total_memories = stats.total_memories, "Memory system ready");
            Some(m)
        }
        Err(e) => {
            warn!("Memory system unavailable: {e}");
            None
        }
    };

    let http = reqwest::Client::new();
    let base_url = format!("https://api.telegram.org/bot{bot_token}");
    let poll_timeout = config.telegram.poll_timeout_secs;
    let allowed_ids = &config.telegram.allowed_user_ids;

    // Per-user sessions keyed by Telegram user ID.
    let mut sessions: HashMap<i64, ConversationSession> = HashMap::new();
    let mut offset: Option<i64> = None;

    info!(
        bot_token_len = bot_token.len(),
        provider = %config.llm.provider,
        model = %config.llm.model,
        "Sam Telegram bot started"
    );
    eprintln!("Sam Telegram bot is running. Send a message to @Sam_N_dgx_bot");

    loop {
        // Long-poll for updates.
        let updates = match get_updates(&http, &base_url, offset, poll_timeout).await {
            Ok(u) => u,
            Err(e) => {
                error!("Telegram getUpdates error: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        for update in updates {
            // Advance offset past this update.
            offset = Some(update.update_id + 1);

            let message = match update.message {
                Some(m) => m,
                None => continue,
            };

            let text = match message.text {
                Some(t) => t,
                None => continue,
            };

            let user = match message.from {
                Some(u) => u,
                None => continue,
            };

            let chat_id = message.chat.id;

            // Access control.
            if !allowed_ids.is_empty() && !allowed_ids.contains(&user.id) {
                warn!(
                    user_id = user.id,
                    username = ?user.username,
                    "rejected message from unauthorized user"
                );
                let _ = send_message(
                    &http,
                    &base_url,
                    chat_id,
                    "죄송합니다. 권한이 없습니다.",
                )
                .await;
                continue;
            }

            let display_name = user
                .username
                .as_deref()
                .unwrap_or(&user.first_name);

            info!(
                user_id = user.id,
                username = %display_name,
                text_len = text.len(),
                "incoming Telegram message"
            );

            // Handle bot commands.
            if text.starts_with('/') {
                let reply = handle_command(&text, &mut memory);
                if let Some(reply) = reply {
                    let _ = send_message(&http, &base_url, chat_id, &reply).await;
                    continue;
                }
                // Unknown command — fall through to LLM.
            }

            // Get or create session for this user.
            let session = sessions
                .entry(user.id)
                .or_insert_with(|| {
                    ConversationSession::new(
                        &format!("tg:{}", user.id),
                        system_prompt.clone(),
                        max_history,
                    )
                });

            // Send typing indicator.
            let _ = send_chat_action(&http, &base_url, chat_id).await;

            // Get reply from LLM.
            let reply = match session
                .reply(
                    client.as_ref(),
                    &mut budget,
                    &text,
                    memory.as_mut(),
                    &config,
                )
                .await
            {
                Ok(text) => text,
                Err(e) => {
                    error!(user_id = user.id, "LLM error: {e}");
                    format!("⚠️ 오류가 발생했어: {e}")
                }
            };

            info!(
                user_id = user.id,
                reply_len = reply.len(),
                "reply ready"
            );

            // Telegram message limit is 4096 chars — split if needed.
            for part in split_telegram_message(&reply, 4096) {
                if let Err(e) = send_message(&http, &base_url, chat_id, &part).await {
                    error!("failed to send Telegram message: {e}");
                }
            }
        }
    }
}

// ── Bot commands ───────────────────────────────────────────────────────

/// Handle slash commands. Returns Some(reply) if handled, None to pass to LLM.
fn handle_command(text: &str, memory: &mut Option<MemoryAdapter>) -> Option<String> {
    let parts: Vec<&str> = text.splitn(2, ' ').collect();
    let cmd = parts[0];

    match cmd {
        "/start" => Some("안녕! 나는 Sam이야. 무엇을 도와줄까? 😊\n\n명령어:\n/memory — 기억 시스템 상태\n/recent — 최근 기억 5개\n/dream — 기억 정리 (꿈)\n/help — 도움말".to_string()),

        "/help" => Some(
            "🧠 Sam 명령어:\n\n\
             /memory — 기억 시스템 통계\n\
             /recent — 최근 기억 보기\n\
             /dream — 기억 정리 실행 (꿈 모드)\n\
             /recall <검색어> — 기억 검색\n\n\
             일반 메시지를 보내면 대화할 수 있어!"
                .to_string(),
        ),

        "/memory" => {
            if let Some(mem) = memory.as_ref() {
                let stats = mem.stats();
                Some(format!(
                    "🧠 Memory-Brain 상태:\n\n\
                     기억 수: {}\n\
                     개념 수: {}\n\
                     해마(Hippocampus): {}\n\
                     신피질(Neocortex): {}\n\
                     꿈(Dream): {}\n\
                     마지막 정리: {}",
                    stats.total_memories,
                    stats.total_concepts,
                    if stats.hippocampus_active { "✅" } else { "❌" },
                    if stats.neocortex_active { "✅" } else { "❌" },
                    if stats.dream_active { "🌙 실행중" } else { "💤 대기" },
                    stats.dream_last_run
                        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_else(|| "없음".to_string()),
                ))
            } else {
                Some("⚠️ 기억 시스템이 비활성화되어 있습니다.".to_string())
            }
        }

        "/recent" => {
            if let Some(mem) = memory.as_ref() {
                let recent = mem.recent(5);
                if recent.is_empty() {
                    Some("기억이 아직 없어.".to_string())
                } else {
                    let mut out = "📝 최근 기억:\n\n".to_string();
                    for (i, hit) in recent.iter().enumerate() {
                        let short = if hit.text.len() > 100 {
                            format!("{}...", &hit.text[..100])
                        } else {
                            hit.text.clone()
                        };
                        out.push_str(&format!(
                            "{}. [강도 {:.2}] {}\n",
                            i + 1,
                            hit.similarity,
                            short.replace('\n', " | "),
                        ));
                    }
                    Some(out)
                }
            } else {
                Some("⚠️ 기억 시스템이 비활성화되어 있습니다.".to_string())
            }
        }

        "/dream" => {
            if let Some(mem) = memory.as_mut() {
                let result = mem.dream();
                Some(format!("🌙 {result}"))
            } else {
                Some("⚠️ 기억 시스템이 비활성화되어 있습니다.".to_string())
            }
        }

        "/recall" => {
            let query = parts.get(1).unwrap_or(&"").trim();
            if query.is_empty() {
                return Some("사용법: /recall <검색어>".to_string());
            }
            if let Some(mem) = memory.as_mut() {
                let hits = mem.recall(query, 5);
                if hits.is_empty() {
                    Some("관련 기억을 찾지 못했어.".to_string())
                } else {
                    let mut out = format!("🔍 '{query}' 검색 결과:\n\n");
                    for (i, hit) in hits.iter().enumerate() {
                        out.push_str(&format!(
                            "{}. [유사도 {:.2}] {}\n",
                            i + 1,
                            hit.similarity,
                            hit.text.replace('\n', " | "),
                        ));
                    }
                    Some(out)
                }
            } else {
                Some("⚠️ 기억 시스템이 비활성화되어 있습니다.".to_string())
            }
        }

        _ => None, // Unknown command — pass to LLM.
    }
}

// ── Telegram API types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TgResponse<T> {
    ok: bool,
    #[serde(default)]
    description: Option<String>,
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
struct TgUpdate {
    update_id: i64,
    message: Option<TgMessage>,
}

#[derive(Debug, Deserialize)]
struct TgMessage {
    chat: TgChat,
    from: Option<TgUser>,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TgChat {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct TgUser {
    id: i64,
    first_name: String,
    #[serde(default)]
    username: Option<String>,
}

#[derive(Debug, Serialize)]
struct SendMessageRequest<'a> {
    chat_id: i64,
    text: &'a str,
}

#[derive(Debug, Serialize)]
struct SendChatActionRequest {
    chat_id: i64,
    action: &'static str,
}

// ── Telegram API helpers ───────────────────────────────────────────────

async fn get_updates(
    http: &reqwest::Client,
    base_url: &str,
    offset: Option<i64>,
    timeout: u64,
) -> anyhow::Result<Vec<TgUpdate>> {
    let mut params = vec![
        ("timeout".to_string(), timeout.to_string()),
        ("allowed_updates".to_string(), "[\"message\"]".to_string()),
    ];
    if let Some(off) = offset {
        params.push(("offset".to_string(), off.to_string()));
    }

    let resp: TgResponse<Vec<TgUpdate>> = http
        .get(format!("{base_url}/getUpdates"))
        .query(&params)
        .timeout(std::time::Duration::from_secs(timeout + 10))
        .send()
        .await?
        .json()
        .await?;

    if !resp.ok {
        anyhow::bail!(
            "Telegram API error: {}",
            resp.description.unwrap_or_default()
        );
    }

    Ok(resp.result.unwrap_or_default())
}

async fn send_message(
    http: &reqwest::Client,
    base_url: &str,
    chat_id: i64,
    text: &str,
) -> anyhow::Result<()> {
    let body = SendMessageRequest { chat_id, text };
    let resp: TgResponse<serde_json::Value> = http
        .post(format!("{base_url}/sendMessage"))
        .json(&body)
        .send()
        .await?
        .json()
        .await?;

    if !resp.ok {
        anyhow::bail!(
            "sendMessage failed: {}",
            resp.description.unwrap_or_default()
        );
    }
    Ok(())
}

async fn send_chat_action(
    http: &reqwest::Client,
    base_url: &str,
    chat_id: i64,
) -> anyhow::Result<()> {
    let body = SendChatActionRequest {
        chat_id,
        action: "typing",
    };
    let _ = http
        .post(format!("{base_url}/sendChatAction"))
        .json(&body)
        .send()
        .await;
    Ok(())
}

// ── Helpers ────────────────────────────────────────────────────────────

fn split_telegram_message(text: &str, max_len: usize) -> Vec<String> {
    if text.chars().count() <= max_len {
        return vec![text.to_string()];
    }

    let mut parts = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.chars().count() <= max_len {
            parts.push(remaining.to_string());
            break;
        }

        let byte_limit = remaining
            .char_indices()
            .nth(max_len)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());

        let chunk = &remaining[..byte_limit];
        let split_at = chunk
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(byte_limit);

        parts.push(remaining[..split_at].to_string());
        remaining = &remaining[split_at..];
    }

    parts
}

fn load_token(source: &str) -> anyhow::Result<String> {
    if let Some(var_name) = source.strip_prefix("env:") {
        let key = std::env::var(var_name)
            .map_err(|_| anyhow::anyhow!("env var '{var_name}' not set"))?
            .trim()
            .to_string();
        if key.is_empty() {
            anyhow::bail!("env var '{var_name}' is empty");
        }
        return Ok(key);
    }

    if let Some(file_path) = source.strip_prefix("file:") {
        let expanded = sam_core::expand_tilde(file_path);
        let key = std::fs::read_to_string(&expanded)
            .map_err(|e| anyhow::anyhow!("file '{expanded}' not readable: {e}"))?
            .trim()
            .to_string();
        if key.is_empty() {
            anyhow::bail!("file '{expanded}' is empty");
        }
        return Ok(key);
    }

    anyhow::bail!("invalid token source (need env: or file: prefix)")
}
