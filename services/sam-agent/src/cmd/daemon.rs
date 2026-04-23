//! `sam daemon` — long-running iMessage agent backed by Claude API.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use sam_claude::{
    load_api_key, load_system_prompt, ConversationSession, LlmBackend, OpenAiCompatibleClient,
    SamClaudeClient, TokenBudget, XaiClient,
};
use sam_core::{config_path, load_config};
use sam_memory_adapter::MemoryAdapter;
use sam_imessage::outbound::run_sender;
use sam_imessage::poller::run_poller;
use sam_imessage::types::{IncomingMessage, OutgoingMessage};

/// Maximum length of a single iMessage before splitting.
const MSG_SPLIT_LEN: usize = 500;

pub async fn run() -> i32 {
    let config = match load_config(config_path()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            return 2;
        }
    };

    if config.imessage.allowed_handles.is_empty() {
        eprintln!("error: [imessage].allowed_handles is empty — no one to talk to");
        return 2;
    }

    // Load API key.
    let api_key = match load_api_key(&config.llm) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("API key error: {e}");
            return 2;
        }
    };

    let client: Arc<dyn LlmBackend> = match config.llm.provider.as_str() {
        "xai" => match XaiClient::new(api_key, &config.llm) {
            Ok(c) => Arc::new(c),
            Err(e) => {
                eprintln!("HTTP client error: {e}");
                return 2;
            }
        },
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

    // Long-term memory (optional — daemon runs without it).
    let mut memory: Option<MemoryAdapter> = match MemoryAdapter::from_config(&config.memory) {
        Ok(m) => {
            let stats = m.stats();
            info!(total_memories = stats.total_memories, "Memory system ready");
            Some(m)
        }
        Err(e) => {
            warn!("Memory system unavailable, running without long-term memory: {e}");
            None
        }
    };

    info!(provider = %config.llm.provider, model = %config.llm.model, "Sam daemon started");

    let cancel = CancellationToken::new();

    // Channels.
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<IncomingMessage>(64);
    let (outbound_tx, outbound_rx) = mpsc::channel::<OutgoingMessage>(64);

    // Poller task.
    let poller_cancel = cancel.clone();
    let imsg_config = config.imessage.clone();
    let poller_handle = tokio::spawn(async move {
        if let Err(e) = run_poller(imsg_config, inbound_tx, poller_cancel).await {
            error!("poller error: {e}");
        }
    });

    // Sender task.
    let sender_cancel = cancel.clone();
    let rate = config.imessage.send_rate_limit_ms;
    let sender_handle = tokio::spawn(async move {
        if let Err(e) = run_sender(rate, outbound_rx, sender_cancel).await {
            error!("sender error: {e}");
        }
    });

    // Claude router task — runs on the main spawn so it can own mutable
    // sessions and budget without Arc<Mutex>.
    let router_cancel = cancel.clone();
    let router_client = Arc::clone(&client);
    let router_config = config.clone();
    let router_handle = tokio::spawn(async move {
        let mut sessions: HashMap<String, ConversationSession> = HashMap::new();
        // Dedup map: tracks texts we sent + when, so we can skip echo copies.
        // Entries expire after SENT_TEXT_TTL to prevent unbounded growth.
        let mut sent_texts: HashMap<String, std::time::Instant> = HashMap::new();
        const SENT_TEXT_TTL_SECS: u64 = 120;
        // Track consecutive errors per handle — reset session after too many.
        let mut consecutive_errors: HashMap<String, u32> = HashMap::new();
        const MAX_CONSECUTIVE_ERRORS: u32 = 3;
        // Cooldown: timestamp of last reply sent per handle.
        // Ignore incoming messages within ECHO_COOLDOWN_MS of our last reply.
        let mut last_reply_time: HashMap<String, std::time::Instant> = HashMap::new();
        const ECHO_COOLDOWN_MS: u64 = 3000;

        loop {
            tokio::select! {
                _ = router_cancel.cancelled() => break,
                msg = inbound_rx.recv() => match msg {
                    Some(m) => {
                        // Cooldown: skip messages arriving too soon after
                        // we sent a reply — they are almost certainly echoes
                        // of our own messages seen by chat.db.
                        if let Some(t) = last_reply_time.get(&m.sender) {
                            if t.elapsed() < std::time::Duration::from_millis(ECHO_COOLDOWN_MS) {
                                info!(
                                    sender = %m.sender,
                                    rowid = m.rowid,
                                    "skipping message during echo cooldown"
                                );
                                continue;
                            }
                        }

                        // Skip messages that are echoes of our own replies.
                        // AppleScript `return` inserts \r, so we normalise
                        // before comparison.
                        let normalised = normalize_for_dedup(&m.text);
                        if let Some(ts) = sent_texts.get(&normalised) {
                            if ts.elapsed().as_secs() < SENT_TEXT_TTL_SECS {
                                info!(
                                    sender = %m.sender,
                                    rowid = m.rowid,
                                    "skipping echo of own reply"
                                );
                                continue;
                            }
                        }

                        sessions
                            .entry(m.sender.clone())
                            .or_insert_with(|| {
                                ConversationSession::new(
                                    &m.sender,
                                    system_prompt.clone(),
                                    max_history,
                                )
                            });

                        // Check if this handle has too many consecutive errors.
                        let err_count = consecutive_errors.get(&m.sender).copied().unwrap_or(0);
                        if err_count >= MAX_CONSECUTIVE_ERRORS {
                            warn!(
                                sender = %m.sender,
                                errors = err_count,
                                "too many consecutive errors, resetting session"
                            );
                            sessions.remove(&m.sender);
                            consecutive_errors.remove(&m.sender);
                        }

                        // Ensure session exists (may have been removed above).
                        sessions.entry(m.sender.clone()).or_insert_with(|| {
                            ConversationSession::new(
                                &m.sender,
                                system_prompt.clone(),
                                max_history,
                            )
                        });
                        let session = sessions.get_mut(&m.sender).unwrap();

                        let reply = match session.reply(
                            router_client.as_ref(),
                            &mut budget,
                            &m.text,
                            memory.as_mut(),
                            &router_config,
                        ).await {
                            Ok(text) => {
                                consecutive_errors.remove(&m.sender);
                                text
                            }
                            Err(e) => {
                                let count = consecutive_errors
                                    .entry(m.sender.clone())
                                    .or_insert(0);
                                *count += 1;
                                error!(
                                    sender = %m.sender,
                                    consecutive = *count,
                                    "LLM error: {e}"
                                );
                                if *count >= MAX_CONSECUTIVE_ERRORS {
                                    // Don't send error message to avoid echo loop.
                                    // Session will be reset on next incoming message.
                                    continue;
                                }
                                format!("⚠️ 오류가 발생했어: {e}")
                            }
                        };

                        info!(
                            sender = %m.sender,
                            rowid = m.rowid,
                            reply_len = reply.len(),
                            "reply ready"
                        );

                        // Split long messages for readability.
                        let parts = split_message(&reply, MSG_SPLIT_LEN);
                        let now = std::time::Instant::now();
                        for part in &parts {
                            // Remember what we sent so we can filter the echo.
                            sent_texts.insert(normalize_for_dedup(part), now);
                        }
                        // Evict expired entries to prevent unbounded growth.
                        sent_texts.retain(|_, ts| ts.elapsed().as_secs() < SENT_TEXT_TTL_SECS);
                        for part in parts {
                            let out = OutgoingMessage {
                                handle: m.sender.clone(),
                                body: part,
                            };
                            if outbound_tx.send(out).await.is_err() {
                                return;
                            }
                        }
                        // Record when we last sent a reply to this handle.
                        last_reply_time.insert(m.sender.clone(), std::time::Instant::now());
                    }
                    None => break,
                }
            }
        }
    });

    // Wait for SIGINT (Ctrl+C) or SIGTERM.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("failed to register SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = sigterm.recv() => {},
    }
    info!("Sam shutting down");
    cancel.cancel();

    let _ = tokio::join!(poller_handle, sender_handle, router_handle);
    info!("Sam daemon stopped");
    0
}

/// Normalise text for echo dedup: AppleScript's `return` is `\r`, but our
/// outbound text uses `\n`. Collapse both to `\n` so they compare equal.
fn normalize_for_dedup(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim()
        .trim_end_matches(|c: char| c.is_ascii_punctuation() || c == '!' || c == '？' || c == '！')
        .to_string()
}

/// Split a message into chunks of at most `max_len` *characters*, preferring
/// line-break boundaries. Returns the original text as a single-element vec
/// if it's short enough. Operates on char boundaries to avoid panics on
/// multibyte UTF-8 (Korean, emoji, etc.).
fn split_message(text: &str, max_len: usize) -> Vec<String> {
    let char_count = text.chars().count();
    if char_count <= max_len {
        return vec![text.to_string()];
    }

    let mut parts = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.chars().count() <= max_len {
            parts.push(remaining.to_string());
            break;
        }

        // Find the byte offset of the max_len-th character.
        let byte_limit = remaining
            .char_indices()
            .nth(max_len)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());

        // Try to split at the last newline within the chunk.
        let chunk = &remaining[..byte_limit];
        let split_at = chunk
            .rfind('\n')
            .map(|i| i + 1) // include the newline in this chunk
            .unwrap_or(byte_limit);

        parts.push(remaining[..split_at].to_string());
        remaining = &remaining[split_at..];
    }

    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_message_not_split() {
        let parts = split_message("hello", 500);
        assert_eq!(parts, vec!["hello"]);
    }

    #[test]
    fn long_message_splits_at_newline() {
        let text = "a\n".repeat(300); // 600 chars
        let parts = split_message(&text, 500);
        assert!(parts.len() >= 2);
        for part in &parts {
            assert!(part.len() <= 500);
        }
    }

    #[test]
    fn long_message_without_newlines() {
        let text = "x".repeat(1200);
        let parts = split_message(&text, 500);
        assert_eq!(parts.len(), 3);
    }

    #[test]
    fn korean_text_splits_safely() {
        // 한글 600자 — char-level split이어야 panic 없이 동작
        let text = "가".repeat(600);
        let parts = split_message(&text, 500);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].chars().count(), 500);
        assert_eq!(parts[1].chars().count(), 100);
    }
}
