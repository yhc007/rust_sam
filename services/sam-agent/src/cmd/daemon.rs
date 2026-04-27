//! `sam daemon` — long-running iMessage agent backed by Claude API.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;
use std::fs;

use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use chrono::Timelike;
use serde::Serialize;
use tracing::{error, info, warn};

use sam_claude::{
    load_api_key, load_system_prompt, ConversationSession, LlmBackend, OpenAiCompatibleClient,
    SamClaudeClient, TokenBudget, XaiClient,
};
use sam_claude::whisper;
use sam_core::{config_path, load_config, AgentStore, CronStore, DeliveryQueue, FlowStore, SkillStore, run_hot_reload};
use sam_memory_adapter::MemoryAdapter;
use base64::Engine as _;
use sam_imessage::outbound::run_sender;
use sam_imessage::poller::run_poller;
use sam_imessage::types::{IncomingMessage, OutgoingMessage};

/// Maximum length of a single iMessage before splitting.
const MSG_SPLIT_LEN: usize = 500;

/// Shared runtime stats written to ~/.sam/state/daemon_stats.json periodically.
#[derive(Debug, Clone, Serialize)]
pub struct DaemonStats {
    pub started_at: i64,
    pub uptime_secs: u64,
    pub messages_received: u64,
    pub messages_sent: u64,
    pub active_sessions: usize,
    pub last_message_at: Option<i64>,
    pub errors_total: u64,
    pub whisper_enabled: bool,
    pub mcp_servers: usize,
    pub heartbeat_sent: u64,
}

/// Atomic counters for concurrent access from multiple tasks.
struct StatsCounters {
    messages_received: AtomicU64,
    messages_sent: AtomicU64,
    errors_total: AtomicU64,
    heartbeat_sent: AtomicU64,
    last_message_at: AtomicU64, // unix timestamp or 0
}

impl StatsCounters {
    fn new() -> Self {
        Self {
            messages_received: AtomicU64::new(0),
            messages_sent: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            heartbeat_sent: AtomicU64::new(0),
            last_message_at: AtomicU64::new(0),
        }
    }
}

/// Write stats JSON to the state directory.
fn write_stats(stats: &DaemonStats) {
    let state_dir = sam_core::state_dir();
    let _ = fs::create_dir_all(&state_dir);
    let path = state_dir.join("daemon_stats.json");
    if let Ok(json) = serde_json::to_string_pretty(stats) {
        let _ = fs::write(path, json);
    }
}

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

    // Cron store (reminders & scheduled jobs).
    let cron_store = Arc::new(Mutex::new(CronStore::load()));
    info!(jobs = cron_store.lock().await.list().len(), "CronStore loaded");

    // Flow store (workflow definitions from ~/.sam/flows/).
    let flow_store = Arc::new(Mutex::new(FlowStore::load()));

    // Delivery queue (persist outbound messages across restarts).
    let delivery_queue = Arc::new(Mutex::new(DeliveryQueue::load()));

    // MCP servers (external tool providers).
    let mcp_clients: Arc<Mutex<Vec<sam_claude::mcp::McpClient>>> = {
        let clients = sam_claude::mcp::spawn_all(&config.mcp.servers).await;
        if !clients.is_empty() {
            info!(count = clients.len(), "MCP servers started");
        }
        Arc::new(Mutex::new(clients))
    };

    // Collect MCP tool definitions for sessions.
    let mcp_tool_defs: Vec<sam_claude::types::ToolDefinition> = {
        let clients = mcp_clients.lock().await;
        clients.iter().flat_map(|c| c.cached_tools.clone()).collect()
    };

    let mcp_server_count = {
        let clients = mcp_clients.lock().await;
        clients.len()
    };

    // Custom skill store (user-defined tools from ~/.sam/tools/).
    let skill_store = Arc::new(Mutex::new(SkillStore::load()));

    // Collect skill tool definitions for sessions.
    let skill_tool_defs: Vec<sam_claude::types::ToolDefinition> = {
        let store = skill_store.lock().await;
        store.tool_definitions_raw().into_iter().map(|(name, description, input_schema)| {
            sam_claude::types::ToolDefinition { name, description, input_schema }
        }).collect()
    };

    // Agent store (multi-agent routing).
    let agent_store = Arc::new(Mutex::new(AgentStore::load()));

    // Runtime stats counters.
    let stats = Arc::new(StatsCounters::new());
    let started_at = chrono::Utc::now().timestamp();

    info!(provider = %config.llm.provider, model = %config.llm.model, "Sam daemon started");

    let cancel = CancellationToken::new();

    // Channels.
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<IncomingMessage>(64);
    let (outbound_tx, outbound_rx) = mpsc::channel::<OutgoingMessage>(64);

    // Flush any pending messages from previous run.
    {
        let mut dq = delivery_queue.lock().await;
        let pending = dq.drain_pending();
        if !pending.is_empty() {
            info!(count = pending.len(), "flushing pending delivery queue");
            for msg in pending {
                let out = OutgoingMessage {
                    handle: msg.handle,
                    body: msg.body,
                    attachment: None,
                };
                let _ = outbound_tx.send(out).await;
            }
        }
    }

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

    // Cron runner task — checks for due jobs every 60 seconds.
    let cron_cancel = cancel.clone();
    let cron_outbound_tx = outbound_tx.clone();
    let cron_store_runner = Arc::clone(&cron_store);
    let cron_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = cron_cancel.cancelled() => break,
                _ = interval.tick() => {
                    let now = chrono::Utc::now().timestamp();
                    let mut store = cron_store_runner.lock().await;
                    let due: Vec<_> = store.due_jobs(now).into_iter().cloned().collect();
                    for job in &due {
                        let msg = OutgoingMessage {
                            handle: job.handle.clone(),
                            body: format!("⏰ {}", job.message),
                    attachment: None,
                        };
                        info!(job_id = %job.id, message = %job.message, "cron job fired");
                        let _ = cron_outbound_tx.send(msg).await;
                        store.mark_fired(&job.id, now);
                    }
                    // Remove one-shot jobs that have fired.
                    store.cleanup_fired();
                }
            }
        }
    });

    // Flow runner task — checks for cron-triggered flows every 60 seconds.
    let flow_cancel = cancel.clone();
    let flow_outbound_tx = outbound_tx.clone();
    let flow_store_runner = Arc::clone(&flow_store);
    let flow_client = Arc::clone(&client);
    let flow_config = config.clone();
    let flow_cron_store = Arc::clone(&cron_store);
    let flow_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = flow_cancel.cancelled() => break,
                _ = interval.tick() => {
                    let now = chrono::Utc::now().timestamp();
                    let due_flows = {
                        let store = flow_store_runner.lock().await;
                        store.due_flows(now).into_iter().cloned().collect::<Vec<_>>()
                    };
                    for flow_def in &due_flows {
                        info!(flow = %flow_def.name, "executing cron-triggered flow");
                        let result = sam_claude::run_flow(
                            flow_def,
                            flow_client.as_ref(),
                            None, // no memory in flow runner context
                            &flow_config,
                            Some(Arc::clone(&flow_cron_store)),
                            "system",
                        ).await;
                        // Send outputs from Send steps.
                        if result.success {
                            for step in &flow_def.steps {
                                if let sam_core::FlowStep::Send { name, handle, .. } = step {
                                    if let Some(body) = result.outputs.get(name.as_str()) {
                                        let msg = sam_imessage::types::OutgoingMessage {
                                            handle: handle.clone(),
                                            body: body.clone(),
                    attachment: None,
                                        };
                                        let _ = flow_outbound_tx.send(msg).await;
                                    }
                                }
                            }
                        } else {
                            warn!(
                                flow = %flow_def.name,
                                error = ?result.error,
                                "cron flow execution failed"
                            );
                        }
                    }
                }
            }
        }
    });

    // Pre-clone for the heartbeat task (before router moves these).
    let heartbeat_tx = outbound_tx.clone();
    let heartbeat_cron_store = Arc::clone(&cron_store);
    let router_stats = Arc::clone(&stats);

    // Claude router task — runs on the main spawn so it can own mutable
    // sessions and budget without Arc<Mutex>.
    let router_cancel = cancel.clone();
    let router_client = Arc::clone(&client);
    let router_config = config.clone();
    let router_dq = Arc::clone(&delivery_queue);
    let router_flow_store = Arc::clone(&flow_store);
    let router_mcp_clients = Arc::clone(&mcp_clients);
    let router_mcp_tool_defs = mcp_tool_defs.clone();
    let router_skill_store = Arc::clone(&skill_store);
    let router_skill_tool_defs = skill_tool_defs.clone();
    let router_agent_store = Arc::clone(&agent_store);
    let router_handle = tokio::spawn(async move {
        let mut sessions: HashMap<String, ConversationSession> = HashMap::new();
        // Multi-agent: tracks which agent each handle is currently using.
        let mut active_agents: HashMap<String, String> = HashMap::new();
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
                        router_stats.messages_received.fetch_add(1, Relaxed);
                        router_stats.last_message_at.store(
                            chrono::Utc::now().timestamp() as u64, Relaxed,
                        );

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

                        // ── Multi-agent session routing ───────────────────────
                        let session_key = if router_config.agents.enabled {
                            // Determine active agent for this handle.
                            let agent_name = if let Some(name) = active_agents.get(&m.sender) {
                                name.clone()
                            } else {
                                // Auto-classify: keyword first, then LLM fallback.
                                let store = router_agent_store.lock().await;
                                let keyword_match = store.classify(&m.text)
                                    .map(|s| s.to_string());

                                let classified = if let Some(name) = keyword_match {
                                    name
                                } else if router_config.agents.llm_classify && store.list().len() > 1 {
                                    // LLM-based classification.
                                    let (sys, prompt) = store.build_classify_prompt(
                                        &m.text,
                                        &router_config.agents.default_agent,
                                    );
                                    drop(store); // release lock before async call
                                    let messages = vec![
                                        sam_claude::ChatMessage::text("user", &prompt),
                                    ];
                                    match router_client.chat(&sys, &messages, None).await {
                                        Ok(resp) => {
                                            let store = router_agent_store.lock().await;
                                            store.parse_classify_response(
                                                &resp.text,
                                                &router_config.agents.default_agent,
                                            )
                                        }
                                        Err(e) => {
                                            warn!(error = %e, "LLM classify failed, using default");
                                            router_config.agents.default_agent.clone()
                                        }
                                    }
                                } else {
                                    router_config.agents.default_agent.clone()
                                };

                                if classified != router_config.agents.default_agent {
                                    info!(
                                        sender = %m.sender,
                                        agent = %classified,
                                        "auto-classified to agent"
                                    );
                                }
                                classified
                            };
                            format!("{}::{}", m.sender, agent_name)
                        } else {
                            m.sender.clone()
                        };

                        // Create or load the session.
                        if !sessions.contains_key(&session_key) {
                            let (prompt, filter) = if router_config.agents.enabled {
                                let agent_name = active_agents
                                    .get(&m.sender)
                                    .map(|s| s.as_str())
                                    .unwrap_or(&router_config.agents.default_agent);
                                let store = router_agent_store.lock().await;
                                if let Some(agent_def) = store.get(agent_name) {
                                    (agent_def.load_prompt(), Some(agent_def.tools.clone()))
                                } else {
                                    (system_prompt.clone(), None)
                                }
                            } else {
                                (system_prompt.clone(), None)
                            };

                            let mut s = if let Some(ref f) = filter {
                                ConversationSession::new_with_filter(
                                    &session_key, prompt, max_history, f,
                                )
                            } else {
                                ConversationSession::load(
                                    &session_key, prompt, max_history,
                                )
                            };
                            if !router_mcp_tool_defs.is_empty() {
                                s.add_tools(router_mcp_tool_defs.clone());
                            }
                            if !router_skill_tool_defs.is_empty() {
                                s.add_tools(router_skill_tool_defs.clone());
                            }
                            sessions.insert(session_key.clone(), s);
                        }

                        // Check consecutive errors → reset if needed.
                        let err_count = consecutive_errors.get(&m.sender).copied().unwrap_or(0);
                        if err_count >= MAX_CONSECUTIVE_ERRORS {
                            warn!(
                                sender = %m.sender,
                                errors = err_count,
                                "too many consecutive errors, resetting session"
                            );
                            sessions.remove(&session_key);
                            consecutive_errors.remove(&m.sender);
                            // Re-create fresh.
                            let mut s = ConversationSession::new(
                                &session_key, system_prompt.clone(), max_history,
                            );
                            if !router_mcp_tool_defs.is_empty() {
                                s.add_tools(router_mcp_tool_defs.clone());
                            }
                            if !router_skill_tool_defs.is_empty() {
                                s.add_tools(router_skill_tool_defs.clone());
                            }
                            sessions.insert(session_key.clone(), s);
                        }

                        let session = sessions.get_mut(&session_key).unwrap();

                        // Spawn ack timer — sends "..." if reply takes too long.
                        let ack_delay = router_config.llm.ack_delay_secs;
                        let ack_task = if ack_delay > 0 {
                            let ack_tx = outbound_tx.clone();
                            let ack_handle = m.sender.clone();
                            Some(tokio::spawn(async move {
                                tokio::time::sleep(Duration::from_secs(ack_delay)).await;
                                let _ = ack_tx.send(OutgoingMessage {
                                    handle: ack_handle,
                                    body: "...".to_string(),
                                    attachment: None,
                                }).await;
                            }))
                        } else {
                            None
                        };

                        // Encode image attachments as base64 for Claude Vision.
                        let images: Vec<(String, String)> = m.attachments.iter().filter_map(|att| {
                            let path = std::path::Path::new(&att.path);
                            // Skip files > 10 MB.
                            match fs::metadata(path) {
                                Ok(meta) if meta.len() > 10 * 1024 * 1024 => {
                                    warn!(path = %att.path, "skipping attachment > 10MB");
                                    return None;
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    warn!(path = %att.path, "cannot read attachment metadata: {e}");
                                    return None;
                                }
                            }
                            match fs::read(path) {
                                Ok(bytes) => {
                                    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
                                    Some((att.mime_type.clone(), encoded))
                                }
                                Err(e) => {
                                    warn!(path = %att.path, "failed to read attachment: {e}");
                                    None
                                }
                            }
                        }).collect();

                        // Transcribe audio attachments via Whisper STT.
                        let mut user_text = m.text.clone();
                        if router_config.whisper.enabled {
                            for att in &m.attachments {
                                if !att.mime_type.starts_with("audio/") {
                                    continue;
                                }
                                let path = std::path::Path::new(&att.path);
                                info!(path = %att.path, mime = %att.mime_type, "transcribing audio attachment");
                                match whisper::transcribe_audio(path, &router_config).await {
                                    Ok(transcript) => {
                                        info!(len = transcript.len(), "audio transcribed");
                                        if user_text.is_empty() {
                                            user_text = transcript;
                                        } else {
                                            user_text = format!("{user_text}\n\n[음성 메시지]: {transcript}");
                                        }
                                    }
                                    Err(e) => {
                                        warn!(path = %att.path, "whisper transcription failed: {e}");
                                    }
                                }
                            }
                        }

                        // Slash command interception — handle locally without LLM.
                        let slash_result = super::slash_commands::try_handle(
                            &user_text,
                            &router_config,
                            &budget,
                            Some(&cron_store),
                            Some(&router_flow_store),
                            Some(&router_skill_store),
                        ).await;

                        if let super::slash_commands::SlashResult::Handled(response) = slash_result {
                            // Cancel ack timer since we respond immediately.
                            if let Some(t) = ack_task { t.abort(); }

                            // Handle /clear specially — reset the session.
                            if user_text.trim() == "/clear" || user_text.trim() == "/초기화" {
                                sessions.remove(&session_key);
                                active_agents.remove(&m.sender);
                            }

                            // Handle /agent switch — update active agent.
                            if response.starts_with("__AGENT_SWITCH__:") {
                                if let Some(agent_name) = response.lines().next()
                                    .and_then(|l| l.strip_prefix("__AGENT_SWITCH__:"))
                                {
                                    let prev = active_agents.get(&m.sender).cloned();
                                    active_agents.insert(m.sender.clone(), agent_name.to_string());
                                    info!(
                                        sender = %m.sender,
                                        from = ?prev,
                                        to = agent_name,
                                        "agent switched via /agent command"
                                    );
                                }
                                // Strip the sentinel line from the response.
                                let display_response: String = response.lines()
                                    .filter(|l| !l.starts_with("__AGENT_SWITCH__:"))
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                let parts = split_message(&display_response, MSG_SPLIT_LEN);
                                let now = std::time::Instant::now();
                                for part in &parts {
                                    sent_texts.insert(normalize_for_dedup(part), now);
                                }
                                sent_texts.retain(|_, ts| ts.elapsed().as_secs() < SENT_TEXT_TTL_SECS);
                                for part in parts {
                                    let queue_id = {
                                        let mut dq = router_dq.lock().await;
                                        dq.enqueue(&m.sender, &part)
                                    };
                                    let out = OutgoingMessage {
                                        handle: m.sender.clone(),
                                        body: part,
                    attachment: None,
                                    };
                                    if outbound_tx.send(out).await.is_err() {
                                        return;
                                    }
                                    let mut dq = router_dq.lock().await;
                                    dq.ack(&queue_id);
                                }
                                router_stats.messages_sent.fetch_add(1, Relaxed);
                                last_reply_time.insert(m.sender.clone(), std::time::Instant::now());
                                continue;
                            }

                            // Send the slash command response.
                            let parts = split_message(&response, MSG_SPLIT_LEN);
                            let now = std::time::Instant::now();
                            for part in &parts {
                                sent_texts.insert(normalize_for_dedup(part), now);
                            }
                            sent_texts.retain(|_, ts| ts.elapsed().as_secs() < SENT_TEXT_TTL_SECS);
                            for part in parts {
                                let queue_id = {
                                    let mut dq = router_dq.lock().await;
                                    dq.enqueue(&m.sender, &part)
                                };
                                let out = OutgoingMessage {
                                    handle: m.sender.clone(),
                                    body: part,
                    attachment: None,
                                };
                                if outbound_tx.send(out).await.is_err() {
                                    return;
                                }
                                let mut dq = router_dq.lock().await;
                                dq.ack(&queue_id);
                            }
                            router_stats.messages_sent.fetch_add(1, Relaxed);
                            last_reply_time.insert(m.sender.clone(), std::time::Instant::now());
                            continue;
                        }

                        let reply = match session.reply(
                            router_client.as_ref(),
                            &mut budget,
                            &user_text,
                            &images,
                            memory.as_mut(),
                            &router_config,
                            Some(Arc::clone(&cron_store)),
                            Some(Arc::clone(&router_flow_store)),
                            Some(Arc::clone(&router_mcp_clients)),
                            Some(Arc::clone(&router_skill_store)),
                        ).await {
                            Ok(text) => {
                                consecutive_errors.remove(&m.sender);
                                session.save();
                                text
                            }
                            Err(e) => {
                                let count = consecutive_errors
                                    .entry(m.sender.clone())
                                    .or_insert(0);
                                *count += 1;
                                router_stats.errors_total.fetch_add(1, Relaxed);
                                error!(
                                    sender = %m.sender,
                                    consecutive = *count,
                                    "LLM error: {e}"
                                );
                                if *count >= MAX_CONSECUTIVE_ERRORS {
                                    if let Some(t) = ack_task { t.abort(); }
                                    continue;
                                }
                                format!("⚠️ 오류가 발생했어: {e}")
                            }
                        };

                        // Cancel ack if reply arrived before timeout.
                        if let Some(t) = ack_task { t.abort(); }

                        // Multi-agent handoff detection.
                        let reply = if reply.contains("__HANDOFF__:") {
                            if let Some(handoff_line) = reply.lines().find(|l| l.contains("__HANDOFF__:")) {
                                let parts: Vec<&str> = handoff_line.splitn(3, ':').collect();
                                if parts.len() >= 2 {
                                    let target_agent = parts[1];
                                    let context = parts.get(2).unwrap_or(&"");
                                    active_agents.insert(m.sender.clone(), target_agent.to_string());
                                    info!(
                                        sender = %m.sender,
                                        from_agent = "current",
                                        to_agent = target_agent,
                                        "agent handoff"
                                    );
                                    format!("🔀 {} 에이전트로 전환합니다.{}", target_agent,
                                        if context.is_empty() { String::new() }
                                        else { format!("\n(맥락: {context})") }
                                    )
                                } else {
                                    reply
                                }
                            } else {
                                reply
                            }
                        } else {
                            reply
                        };

                        // Attachment detection: extract __ATTACHMENT__:/path lines.
                        let mut attachment_path: Option<String> = None;
                        let reply = if reply.contains("__ATTACHMENT__:") {
                            let mut clean_lines = Vec::new();
                            for line in reply.lines() {
                                if let Some(path) = line.strip_prefix("__ATTACHMENT__:") {
                                    attachment_path = Some(path.to_string());
                                } else {
                                    clean_lines.push(line);
                                }
                            }
                            clean_lines.join("\n")
                        } else {
                            reply
                        };

                        info!(
                            sender = %m.sender,
                            rowid = m.rowid,
                            reply_len = reply.len(),
                            attachment = ?attachment_path,
                            "reply ready"
                        );

                        // If we have an attachment, send it first.
                        if let Some(ref att_path) = attachment_path {
                            let att_msg = OutgoingMessage {
                                handle: m.sender.clone(),
                                body: String::new(),
                                attachment: Some(att_path.clone()),
                            };
                            if outbound_tx.send(att_msg).await.is_err() {
                                return;
                            }
                            router_stats.messages_sent.fetch_add(1, Relaxed);
                        }

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
                            // Enqueue to delivery queue (persists to disk).
                            let queue_id = {
                                let mut dq = router_dq.lock().await;
                                dq.enqueue(&m.sender, &part)
                            };
                            let out = OutgoingMessage {
                                handle: m.sender.clone(),
                                body: part,
                    attachment: None,
                            };
                            if outbound_tx.send(out).await.is_err() {
                                return;
                            }
                            // Ack after successful channel send.
                            let mut dq = router_dq.lock().await;
                            dq.ack(&queue_id);
                        }
                        router_stats.messages_sent.fetch_add(1, Relaxed);
                        // Record when we last sent a reply to this handle.
                        last_reply_time.insert(m.sender.clone(), std::time::Instant::now());
                    }
                    None => break,
                }
            }
        }
    });

    // Hot-reload task — watches config.toml and flows/ for changes.
    let reload_cancel = cancel.clone();
    let shared_config = Arc::new(Mutex::new(config.clone()));
    let reload_flow_store = Arc::clone(&flow_store);
    let reload_skill_store = Arc::clone(&skill_store);
    let reload_handle = tokio::spawn(async move {
        // Spawn a sub-task that periodically reloads the skill store.
        let skill_cancel = reload_cancel.clone();
        let skill_store_ref = reload_skill_store;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            loop {
                tokio::select! {
                    _ = skill_cancel.cancelled() => break,
                    _ = interval.tick() => {
                        let mut store = skill_store_ref.lock().await;
                        store.reload();
                    }
                }
            }
        });

        run_hot_reload(
            shared_config,
            reload_flow_store,
            reload_cancel,
            Duration::from_secs(10),
        )
        .await;
    });

    // Stats writer task — periodically flush runtime stats to disk.
    let stats_cancel = cancel.clone();
    let stats_arc = Arc::clone(&stats);
    let stats_whisper_enabled = config.whisper.enabled;
    let stats_mcp_count = mcp_server_count;
    let stats_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        interval.tick().await; // skip immediate
        loop {
            tokio::select! {
                _ = stats_cancel.cancelled() => break,
                _ = interval.tick() => {
                    let now = chrono::Utc::now().timestamp();
                    let snap = DaemonStats {
                        started_at,
                        uptime_secs: (now - started_at).max(0) as u64,
                        messages_received: stats_arc.messages_received.load(Relaxed),
                        messages_sent: stats_arc.messages_sent.load(Relaxed),
                        active_sessions: 0, // updated below via file count
                        last_message_at: {
                            let ts = stats_arc.last_message_at.load(Relaxed);
                            if ts == 0 { None } else { Some(ts as i64) }
                        },
                        errors_total: stats_arc.errors_total.load(Relaxed),
                        whisper_enabled: stats_whisper_enabled,
                        mcp_servers: stats_mcp_count,
                        heartbeat_sent: stats_arc.heartbeat_sent.load(Relaxed),
                    };
                    write_stats(&snap);
                }
            }
        }
    });

    // Proactive heartbeat task — full autonomous mode.
    // Sam initiates contact: morning brief, reminder nudges, evening summary.
    let heartbeat_cancel = cancel.clone();
    let heartbeat_outbound_tx = heartbeat_tx;
    let heartbeat_handles = config.imessage.allowed_handles.clone();
    let heartbeat_client = Arc::clone(&client);
    let heartbeat_stats = Arc::clone(&stats);
    let hb_config = config.heartbeat.clone();
    let heartbeat_handle = tokio::spawn(async move {
        if !hb_config.enabled {
            info!("heartbeat disabled in config");
            heartbeat_cancel.cancelled().await;
            return;
        }

        let mut interval = tokio::time::interval(Duration::from_secs(hb_config.interval_secs));
        interval.tick().await; // skip immediate

        let mut last_brief_date: Option<chrono::NaiveDate> = None;
        let mut last_evening_date: Option<chrono::NaiveDate> = None;
        // Track nudged reminder IDs to avoid repeat nudges.
        let mut nudged_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

        loop {
            tokio::select! {
                _ = heartbeat_cancel.cancelled() => break,
                _ = interval.tick() => {
                    let now = chrono::Local::now();
                    let hour = now.hour();
                    let now_unix = chrono::Utc::now().timestamp();

                    // Only during waking hours.
                    if !(hb_config.wake_hour..hb_config.sleep_hour).contains(&hour) {
                        continue;
                    }

                    let today = now.date_naive();

                    // ── Morning Brief ──
                    if hour == hb_config.morning_hour && last_brief_date != Some(today) {
                        last_brief_date = Some(today);

                        let reminders_summary = {
                            let store = heartbeat_cron_store.lock().await;
                            let jobs = store.list();
                            if jobs.is_empty() {
                                "예정된 리마인더 없음".to_string()
                            } else {
                                jobs.iter()
                                    .map(|j| format!("- {}", j.message))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            }
                        };

                        let prompt = format!(
                            "너는 Paul의 친구 Sam이야. 지금 아침 {hour}시야.\n\
                             오늘 예정된 리마인더:\n{reminders_summary}\n\n\
                             Paul에게 보낼 짧은 아침 인사 메시지를 작성해. \
                             오늘 할 일이 있으면 간단히 알려주고, 없으면 가벼운 인사만. \
                             반말로 친근하게, 2-3문장 이내로."
                        );

                        if let Some(text) = heartbeat_ask_llm(heartbeat_client.as_ref(), &prompt).await {
                            heartbeat_send(&heartbeat_outbound_tx, &heartbeat_handles, &text).await;
                            heartbeat_stats.heartbeat_sent.fetch_add(1, Relaxed);
                            info!(kind = "morning_brief", "heartbeat sent");
                        }
                    }

                    // ── Evening Summary ──
                    if hour == hb_config.evening_hour && last_evening_date != Some(today) {
                        last_evening_date = Some(today);

                        let msgs_today = heartbeat_stats.messages_received.load(Relaxed);
                        let prompt = format!(
                            "너는 Paul의 친구 Sam이야. 지금 저녁 {hour}시야.\n\
                             오늘 통계: 받은 메시지 {msgs_today}개\n\n\
                             Paul에게 보낼 짧은 저녁 마무리 메시지를 작성해. \
                             내일 예정된 일이 있으면 미리 알려주고, 없으면 편히 쉬라고. \
                             반말로 친근하게, 1-2문장."
                        );

                        if let Some(text) = heartbeat_ask_llm(heartbeat_client.as_ref(), &prompt).await {
                            heartbeat_send(&heartbeat_outbound_tx, &heartbeat_handles, &text).await;
                            heartbeat_stats.heartbeat_sent.fetch_add(1, Relaxed);
                            info!(kind = "evening_summary", "heartbeat sent");
                        }
                    }

                    // ── Reminder Nudge — upcoming one-time reminders ──
                    {
                        let store = heartbeat_cron_store.lock().await;
                        let nudge_window = hb_config.nudge_before_mins * 60; // seconds
                        for job in store.list() {
                            if nudged_ids.contains(&job.id) {
                                continue;
                            }
                            if let sam_core::CronSchedule::Once { at_unix } = &job.schedule {
                                let until = at_unix - now_unix;
                                // Nudge if within window and not yet fired.
                                if until > 0 && until <= nudge_window {
                                    nudged_ids.insert(job.id.clone());
                                    let msg = format!(
                                        "리마인더 알림: {}분 후 예정 → {}",
                                        until / 60,
                                        job.message
                                    );
                                    heartbeat_send(
                                        &heartbeat_outbound_tx,
                                        &heartbeat_handles,
                                        &msg,
                                    ).await;
                                    heartbeat_stats.heartbeat_sent.fetch_add(1, Relaxed);
                                    info!(
                                        kind = "nudge",
                                        job_id = %job.id,
                                        minutes_until = until / 60,
                                        "reminder nudge sent"
                                    );
                                }
                            }
                        }
                        // Evict old nudge IDs (keep only IDs that still exist in store).
                        let valid_ids: std::collections::HashSet<String> =
                            store.list().iter().map(|j| j.id.clone()).collect();
                        nudged_ids.retain(|id| valid_ids.contains(id));
                    }
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

    let _ = tokio::join!(
        poller_handle, sender_handle, router_handle, cron_handle,
        flow_handle, reload_handle, stats_handle, heartbeat_handle
    );
    info!("Sam daemon stopped");
    0
}

// ── Heartbeat helpers ─────────────────────────────────────────────────

/// Ask the LLM a single-shot prompt and return the text, or None on error.
async fn heartbeat_ask_llm(client: &dyn sam_claude::LlmBackend, prompt: &str) -> Option<String> {
    let messages = vec![sam_claude::ChatMessage::text("user", prompt)];
    match client
        .chat("너는 Sam이야. Paul의 친구이자 개인 에이전트.", &messages, None)
        .await
    {
        Ok(resp) => {
            let text = resp.text.trim().to_string();
            if text.is_empty() { None } else { Some(text) }
        }
        Err(e) => {
            warn!("heartbeat LLM error: {e}");
            None
        }
    }
}

/// Send a message to all configured handles.
async fn heartbeat_send(
    tx: &mpsc::Sender<OutgoingMessage>,
    handles: &[String],
    text: &str,
) {
    for handle in handles {
        let _ = tx
            .send(OutgoingMessage {
                handle: handle.clone(),
                body: text.to_string(),
                    attachment: None,
            })
            .await;
    }
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
