//! Sam Chat — multi-session web chat server with agent routing, auth, and memory.
//!
//! Provides:
//! - Token-based auth (login/logout/me)
//! - Multi-agent selection
//! - Session CRUD (create/list/delete/rename)
//! - REST-based chat per session
//! - Memory stats + dream consolidation
//! - File upload/download

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Multipart, Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post, delete, patch},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};

use sam_claude::{
    load_api_key, load_system_prompt, ActiveToolTracker, ConversationSession, LlmBackend,
    OpenAiCompatibleClient, SamClaudeClient, TokenBudget, XaiClient, new_tool_tracker,
};
use sam_core::{
    config_path, load_config, AgentStore, CronStore, FlowStore, SamConfig, SkillStore,
};
use sam_memory_adapter::MemoryAdapter;

// ── Standalone entry point (`sam-agent web`) ─────────────────────────

/// Run the web chat server as a standalone command.
pub async fn run(port: u16) -> i32 {
    let config = match load_config(config_path()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            return 2;
        }
    };

    let api_key = match load_api_key(&config.llm) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("API key error: {e}");
            return 2;
        }
    };

    let client: Arc<dyn LlmBackend> = match SamClaudeClient::new(api_key, &config.llm) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("HTTP client error: {e}");
            return 2;
        }
    };

    let fallback_client: Option<Arc<dyn LlmBackend>> =
        config.llm.fallback_config().and_then(|cfg| build_fallback_client(&cfg));

    let memory = MemoryAdapter::from_config(&config.memory).ok();
    let agent_store = Arc::new(Mutex::new(AgentStore::load()));

    let state = Arc::new(Mutex::new(WebAppState {
        sessions: HashMap::new(),
        session_meta: Vec::new(),
        client,
        fallback_client,
        budget: TokenBudget::load_or_new(config.llm.daily_token_budget),
        memory,
        config: config.clone(),
        cron_store: None,
        flow_store: None,
        skill_store: None,
        agent_store,
        auth_tokens: HashMap::new(),
        system_prompt: load_system_prompt(),
        tool_tracker: new_tool_tracker(),
    }));

    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel2 = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        cancel2.cancel();
    });

    if let Err(e) = run_web_server(port, state, cancel).await {
        eprintln!("web server error: {e}");
        return 1;
    }
    0
}

// ── Shared state ──────────────────────────────────────────────────────

/// Shared application state for the web chat server.
pub struct WebAppState {
    pub sessions: HashMap<String, ConversationSession>,
    pub session_meta: Vec<SessionMeta>,
    pub client: Arc<dyn LlmBackend>,
    pub fallback_client: Option<Arc<dyn LlmBackend>>,
    pub budget: TokenBudget,
    pub memory: Option<MemoryAdapter>,
    pub config: SamConfig,
    pub cron_store: Option<Arc<Mutex<CronStore>>>,
    pub flow_store: Option<Arc<Mutex<FlowStore>>>,
    pub skill_store: Option<Arc<Mutex<SkillStore>>>,
    pub agent_store: Arc<Mutex<AgentStore>>,
    pub auth_tokens: HashMap<String, String>, // token -> username
    pub system_prompt: String,
    pub tool_tracker: ActiveToolTracker,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub agent_id: String,
    pub name: Option<String>,
    pub message_count: usize,
    /// Last working directory used by tools in this session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
}

// ── Wire types ────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
struct AttachmentInfo {
    id: String,
    filename: String,
    mime_type: String,
}

#[derive(Serialize)]
struct ChatResponse {
    reply: String,
    attachments: Vec<AttachmentInfo>,
}

#[derive(Serialize)]
struct AgentInfo {
    id: String,
    name: String,
    description: String,
    model: String,
    provider: String,
    namespace: String,
}

#[derive(Serialize)]
struct MemoryStatsResponse {
    total_memories: usize,
    total_concepts: usize,
    hippocampus_active: bool,
    neocortex_active: bool,
    dream_active: bool,
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct AuthResponse {
    authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
}

#[derive(Deserialize)]
struct CreateSessionRequest {
    agent_id: String,
}

#[derive(Deserialize)]
struct RenameSessionRequest {
    name: Option<String>,
}

#[derive(Deserialize)]
struct ChatRequest {
    session_id: String,
    message: String,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct AgentQuery {
    #[serde(default)]
    agent_id: Option<String>,
}

#[derive(Deserialize)]
struct MapQuery {
    session_id: String,
}

// ── Auth helpers ──────────────────────────────────────────────────────

/// Extract auth token from Authorization header or cookie.
fn extract_token(headers: &axum::http::HeaderMap) -> Option<String> {
    // Check Authorization: Bearer <token>
    if let Some(auth) = headers.get("authorization") {
        if let Ok(val) = auth.to_str() {
            if let Some(token) = val.strip_prefix("Bearer ") {
                return Some(token.to_string());
            }
        }
    }
    // Check cookie: sam_token=<token>
    if let Some(cookie) = headers.get("cookie") {
        if let Ok(val) = cookie.to_str() {
            for part in val.split(';') {
                let part = part.trim();
                if let Some(token) = part.strip_prefix("sam_token=") {
                    return Some(token.to_string());
                }
            }
        }
    }
    None
}

fn check_auth(state: &WebAppState, headers: &axum::http::HeaderMap) -> Option<String> {
    // If no password configured, skip auth.
    if state.config.web_chat.password.is_empty() {
        return Some(state.config.web_chat.username.clone());
    }
    let token = extract_token(headers)?;
    state.auth_tokens.get(&token).cloned()
}

// ── Server ────────────────────────────────────────────────────────────

/// Start the web chat server on the given port.
pub async fn run_web_server(
    port: u16,
    state: Arc<Mutex<WebAppState>>,
    cancel: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    let app = Router::new()
        // Static pages
        .route("/", get(index_page))
        .route("/chat", get(index_page))
        .route("/manifest.json", get(manifest_handler))
        .route("/sw.js", get(sw_handler))
        // Auth
        .route("/api/me", get(me_handler))
        .route("/api/login", post(login_handler))
        .route("/api/logout", post(logout_handler))
        // Agents & LLM
        .route("/api/agents", get(agents_handler))
        .route("/api/llm", get(llm_handler))
        // Sessions
        .route("/api/sessions", get(list_sessions_handler))
        .route("/api/sessions", post(create_session_handler))
        .route("/api/sessions/{id}", delete(delete_session_handler))
        .route("/api/sessions/{id}", patch(rename_session_handler))
        // Chat
        .route("/api/chat", post(chat_handler))
        // Memory
        .route("/api/memory", get(memory_handler))
        .route("/api/dream", post(dream_handler))
        // Knowledge graph
        .route("/api/map", get(map_handler))
        // Active tool status
        .route("/api/claude-status", get(claude_status_handler))
        // Files
        .route("/api/upload", post(upload_handler))
        .route("/api/files/{id}", get(file_handler))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    info!(addr = %addr, "Sam Chat web server starting");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await?;

    info!("Sam Chat web server stopped");
    Ok(())
}

// ── Auth handlers ─────────────────────────────────────────────────────

async fn me_handler(
    State(state): State<Arc<Mutex<WebAppState>>>,
    headers: axum::http::HeaderMap,
) -> Json<AuthResponse> {
    let app = state.lock().await;
    if let Some(username) = check_auth(&app, &headers) {
        Json(AuthResponse { authenticated: true, username: Some(username) })
    } else {
        Json(AuthResponse { authenticated: false, username: None })
    }
}

async fn login_handler(
    State(state): State<Arc<Mutex<WebAppState>>>,
    Json(req): Json<LoginRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let mut app = state.lock().await;
    let expected_user = &app.config.web_chat.username;
    let expected_pass = &app.config.web_chat.password;

    if req.username != *expected_user || req.password != *expected_pass {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let token = uuid::Uuid::new_v4().to_string();
    app.auth_tokens.insert(token.clone(), req.username.clone());

    let cookie = format!("sam_token={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=86400");
    Ok((
        [(axum::http::header::SET_COOKIE, cookie)],
        Json(serde_json::json!({ "ok": true })),
    ))
}

async fn logout_handler(
    State(state): State<Arc<Mutex<WebAppState>>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let mut app = state.lock().await;
    if let Some(token) = extract_token(&headers) {
        app.auth_tokens.remove(&token);
    }
    let cookie = "sam_token=; Path=/; HttpOnly; Max-Age=0";
    (
        [(axum::http::header::SET_COOKIE, cookie.to_string())],
        Json(serde_json::json!({ "ok": true })),
    )
}

// ── Agents handler ────────────────────────────────────────────────────

async fn agents_handler(
    State(state): State<Arc<Mutex<WebAppState>>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Vec<AgentInfo>>, StatusCode> {
    let app = state.lock().await;
    if check_auth(&app, &headers).is_none() && !app.config.web_chat.password.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let store = app.agent_store.lock().await;
    let mut agents: Vec<AgentInfo> = store
        .list()
        .into_iter()
        .map(|a| AgentInfo {
            id: a.name.clone(),
            name: a.name.clone(),
            description: a.description.clone(),
            model: app.config.llm.model.clone(),
            provider: app.config.llm.provider.clone(),
            namespace: "default".to_string(),
        })
        .collect();

    // Always include a "default" agent if none exist.
    if agents.is_empty() {
        agents.push(AgentInfo {
            id: "default".to_string(),
            name: "Sam".to_string(),
            description: "Personal AI agent".to_string(),
            model: app.config.llm.model.clone(),
            provider: app.config.llm.provider.clone(),
            namespace: "default".to_string(),
        });
    }

    Ok(Json(agents))
}

async fn llm_handler(
    State(state): State<Arc<Mutex<WebAppState>>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let app = state.lock().await;
    if check_auth(&app, &headers).is_none() && !app.config.web_chat.password.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let cfg = &app.config.llm;
    let mut result = serde_json::json!({
        "primary": {
            "provider": cfg.provider,
            "model": cfg.model,
            "base_url": cfg.base_url,
            "max_tokens": cfg.max_tokens,
            "temperature": cfg.temperature,
            "daily_token_budget": cfg.daily_token_budget,
        },
        "budget": {
            "used": app.budget.used_today,
            "remaining": app.budget.remaining(),
            "limit": cfg.daily_token_budget,
        }
    });

    if let Some(fb) = cfg.fallback_config() {
        result["fallback"] = serde_json::json!({
            "provider": fb.provider,
            "model": fb.model,
            "base_url": fb.base_url,
        });
    }

    if let Some(fast) = cfg.fast_config() {
        result["fast"] = serde_json::json!({
            "provider": fast.provider,
            "model": fast.model,
        });
    }

    // Check if primary LLM endpoint is reachable.
    let base = cfg.base_url.trim_end_matches('/');
    let url = if base.ends_with("/v1") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    };
    let reachable = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    result["primary"]["reachable"] = serde_json::json!(reachable);

    Ok(Json(result))
}

// ── Session handlers ──────────────────────────────────────────────────

async fn list_sessions_handler(
    State(state): State<Arc<Mutex<WebAppState>>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Vec<SessionMeta>>, StatusCode> {
    let app = state.lock().await;
    if check_auth(&app, &headers).is_none() && !app.config.web_chat.password.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(Json(app.session_meta.clone()))
}

async fn create_session_handler(
    State(state): State<Arc<Mutex<WebAppState>>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<CreateSessionRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut app = state.lock().await;
    if check_auth(&app, &headers).is_none() && !app.config.web_chat.password.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let session_id = uuid::Uuid::new_v4().to_string();
    let max_history = app.config.llm.max_history;
    let system_prompt = app.system_prompt.clone();

    // Try to load agent-specific prompt.
    let prompt = {
        let store = app.agent_store.lock().await;
        if let Some(agent) = store.get(&req.agent_id) {
            let agent_prompt = agent.load_prompt();
            if agent_prompt.is_empty() { system_prompt } else { agent_prompt }
        } else {
            system_prompt
        }
    };

    let mut session = ConversationSession::new(&session_id, prompt, max_history);
    session.set_compaction_limits(
        app.config.llm.max_context_tokens,
        app.config.llm.max_summary_chars,
    );

    app.sessions.insert(session_id.clone(), session);
    app.session_meta.push(SessionMeta {
        session_id: session_id.clone(),
        agent_id: req.agent_id,
        name: None,
        message_count: 0,
        working_dir: None,
    });

    Ok(Json(serde_json::json!({ "session_id": session_id })))
}

async fn delete_session_handler(
    State(state): State<Arc<Mutex<WebAppState>>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut app = state.lock().await;
    if check_auth(&app, &headers).is_none() && !app.config.web_chat.password.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    app.sessions.remove(&id);
    app.session_meta.retain(|s| s.session_id != id);

    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn rename_session_handler(
    State(state): State<Arc<Mutex<WebAppState>>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<RenameSessionRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut app = state.lock().await;
    if check_auth(&app, &headers).is_none() && !app.config.web_chat.password.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    if let Some(meta) = app.session_meta.iter_mut().find(|s| s.session_id == id) {
        meta.name = req.name;
    }

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── Chat handler ──────────────────────────────────────────────────────

async fn chat_handler(
    State(state): State<Arc<Mutex<WebAppState>>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, StatusCode> {
    let mut app = state.lock().await;
    if check_auth(&app, &headers).is_none() && !app.config.web_chat.password.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    if !app.sessions.contains_key(&req.session_id) {
        return Err(StatusCode::NOT_FOUND);
    }

    let WebAppState {
        ref mut sessions,
        ref mut session_meta,
        ref client,
        ref fallback_client,
        ref mut budget,
        ref mut memory,
        ref config,
        ref cron_store,
        ref flow_store,
        ref skill_store,
        ref tool_tracker,
        ..
    } = *app;

    let tracker = Some(Arc::clone(tool_tracker));
    let session = sessions.get_mut(&req.session_id).unwrap();

    let reply = match session
        .reply(
            client.as_ref(),
            budget,
            &req.message,
            &[],
            memory.as_mut(),
            config,
            cron_store.clone(),
            flow_store.clone(),
            None,
            skill_store.clone(),
            tracker.clone(),
        )
        .await
    {
        Ok(text) => text,
        Err(e) => {
            if let Some(ref fb) = fallback_client {
                warn!(primary_error = %e, "primary LLM failed, trying fallback");
                match session
                    .reply(
                        fb.as_ref(),
                        budget,
                        &req.message,
                        &[],
                        memory.as_mut(),
                        config,
                        cron_store.clone(),
                        flow_store.clone(),
                        None,
                        skill_store.clone(),
                        tracker.clone(),
                    )
                    .await
                {
                    Ok(text) => {
                        info!("fallback LLM succeeded");
                        text
                    }
                    Err(e2) => {
                        error!("both primary and fallback LLM failed: {e2}");
                        "⚠️ 일시적으로 응답할 수 없어. 잠시 후 다시 말해줘.".to_string()
                    }
                }
            } else {
                error!("LLM error: {e}");
                format!("오류가 발생했어: {e}")
            }
        }
    };

    // Update message count and last working_dir from tool tracker.
    if let Some(meta) = session_meta.iter_mut().find(|s| s.session_id == req.session_id) {
        meta.message_count += 2; // user + assistant
        // Capture latest working_dir from tool tracker (claude_code entries).
        let t = tool_tracker.lock().await;
        if let Some(last) = t.iter().rev().find(|s| !s.working_dir.is_empty() && s.tool != "agentic_loop") {
            meta.working_dir = Some(last.working_dir.clone());
        }
        // Clean up completed entries from tracker.
        drop(t);
        let mut t = tool_tracker.lock().await;
        t.retain(|s| s.phase == "running");
    }

    // Extract __ATTACHMENT__ markers from reply.
    let (clean_reply, attachments) = extract_attachments(&reply);

    Ok(Json(ChatResponse {
        reply: clean_reply,
        attachments,
    }))
}

// ── Claude Code status handler ───────────────────────────────────────

async fn claude_status_handler(
    State(state): State<Arc<Mutex<WebAppState>>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let app = state.lock().await;
    if check_auth(&app, &headers).is_none() && !app.config.web_chat.password.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let tracker = app.tool_tracker.lock().await;
    // Only return entries that are still "running".
    let active: Vec<_> = tracker.iter().filter(|s| s.phase == "running").collect();
    let now = chrono::Utc::now().timestamp();

    Ok(Json(serde_json::json!({
        "active": active,
        "count": active.len(),
        "server_time": now,
    })))
}

// ── Memory handlers ──────────────────────────────────────────────────

async fn memory_handler(
    State(state): State<Arc<Mutex<WebAppState>>>,
    headers: axum::http::HeaderMap,
    Query(_q): Query<AgentQuery>,
) -> Result<Json<MemoryStatsResponse>, StatusCode> {
    let app = state.lock().await;
    if check_auth(&app, &headers).is_none() && !app.config.web_chat.password.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    match &app.memory {
        Some(mem) => {
            let stats = mem.stats();
            Ok(Json(MemoryStatsResponse {
                total_memories: stats.total_memories,
                total_concepts: stats.total_concepts,
                hippocampus_active: stats.hippocampus_active,
                neocortex_active: stats.neocortex_active,
                dream_active: stats.dream_active,
            }))
        }
        None => Ok(Json(MemoryStatsResponse {
            total_memories: 0,
            total_concepts: 0,
            hippocampus_active: false,
            neocortex_active: false,
            dream_active: false,
        })),
    }
}

async fn dream_handler(
    State(state): State<Arc<Mutex<WebAppState>>>,
    headers: axum::http::HeaderMap,
    Json(_body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut app = state.lock().await;
    if check_auth(&app, &headers).is_none() && !app.config.web_chat.password.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    match app.memory.as_mut() {
        Some(mem) => {
            let result = mem.dream();
            Ok(Json(serde_json::json!({ "result": result })))
        }
        None => Ok(Json(serde_json::json!({ "error": "memory system not available" }))),
    }
}

// ── Knowledge graph (map) handler ────────────────────────────────────

async fn map_handler(
    State(state): State<Arc<Mutex<WebAppState>>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<MapQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let app = state.lock().await;
    if check_auth(&app, &headers).is_none() && !app.config.web_chat.password.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Collect conversation text for SPO extraction.
    let conversation_text = app
        .sessions
        .get(&q.session_id)
        .ok_or(StatusCode::NOT_FOUND)?
        .history_text();

    if conversation_text.trim().is_empty() {
        return Ok(Json(serde_json::json!({
            "triple_count": 0,
            "nodes": [],
            "edges": [],
        })));
    }

    // Use LLM to extract SPO triples.
    let extract_prompt = format!(
        "다음 대화에서 주요 사실을 (주어, 술어, 목적어) 트리플로 추출해줘.\n\
         JSON 배열로만 응답해. 형식: [{{\"s\":\"주어\",\"p\":\"술어\",\"o\":\"목적어\"}}]\n\
         최대 20개까지.\n\n---\n{conversation_text}\n---"
    );
    let messages = vec![sam_claude::ChatMessage::text("user", &extract_prompt)];

    // Use fallback client if available (cheaper for extraction).
    let client_ref: &dyn LlmBackend = if let Some(ref fb) = app.fallback_client {
        fb.as_ref()
    } else {
        app.client.as_ref()
    };

    let triples_json = match client_ref
        .chat("SPO 트리플을 추출하는 도우미. JSON만 반환.", &messages, None)
        .await
    {
        Ok(resp) => resp.text,
        Err(e) => {
            warn!("SPO extraction failed: {e}");
            return Ok(Json(serde_json::json!({
                "triple_count": 0,
                "nodes": [],
                "edges": [],
            })));
        }
    };

    // Parse triples and build graph.
    let triples: Vec<serde_json::Value> = extract_json_array(&triples_json);
    let mut nodes_map: HashMap<String, bool> = HashMap::new();
    let mut edges = Vec::new();

    for t in &triples {
        let s = t["s"].as_str().unwrap_or("").to_string();
        let p = t["p"].as_str().unwrap_or("").to_string();
        let o = t["o"].as_str().unwrap_or("").to_string();
        if s.is_empty() || o.is_empty() {
            continue;
        }
        nodes_map.entry(s.clone()).or_insert(true);
        nodes_map.entry(o.clone()).or_insert(true);
        edges.push(serde_json::json!({ "source": s, "target": o, "label": p }));
    }

    let nodes: Vec<serde_json::Value> = nodes_map
        .keys()
        .map(|k| serde_json::json!({ "id": k, "label": k }))
        .collect();

    Ok(Json(serde_json::json!({
        "triple_count": triples.len(),
        "nodes": nodes,
        "edges": edges,
    })))
}

/// Try to extract a JSON array from LLM response text.
fn extract_json_array(text: &str) -> Vec<serde_json::Value> {
    // Try direct parse.
    if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(text.trim()) {
        return arr;
    }
    // Try to find JSON array in the text (between [ and ]).
    if let Some(start) = text.find('[') {
        if let Some(end) = text.rfind(']') {
            if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&text[start..=end]) {
                return arr;
            }
        }
    }
    Vec::new()
}

// ── File handlers ────────────────────────────────────────────────────

async fn upload_handler(
    State(state): State<Arc<Mutex<WebAppState>>>,
    headers: axum::http::HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<AttachmentInfo>, StatusCode> {
    {
        let app = state.lock().await;
        if check_auth(&app, &headers).is_none() && !app.config.web_chat.password.is_empty() {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }

    let uploads_dir = sam_core::state_dir().join("uploads");
    let _ = std::fs::create_dir_all(&uploads_dir);

    let field = multipart
        .next_field()
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?
        .ok_or(StatusCode::BAD_REQUEST)?;

    let filename = field.file_name().unwrap_or("upload").to_string();
    let mime_type = field
        .content_type()
        .unwrap_or("application/octet-stream")
        .to_string();
    let data = field.bytes().await.map_err(|_| StatusCode::BAD_REQUEST)?;

    let id = uuid::Uuid::new_v4().to_string();
    let dest = uploads_dir.join(&id);
    std::fs::write(&dest, &data).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let meta_path = uploads_dir.join(format!("{id}.json"));
    let meta = serde_json::json!({ "filename": filename, "mime_type": mime_type });
    let _ = std::fs::write(&meta_path, meta.to_string());

    Ok(Json(AttachmentInfo { id, filename, mime_type }))
}

async fn file_handler(Path(id): Path<String>) -> Result<impl IntoResponse, StatusCode> {
    let uploads_dir = sam_core::state_dir().join("uploads");
    let file_path = uploads_dir.join(&id);
    let meta_path = uploads_dir.join(format!("{id}.json"));

    if !file_path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }

    let data = std::fs::read(&file_path).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let content_type = if meta_path.exists() {
        std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v["mime_type"].as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "application/octet-stream".to_string())
    } else {
        "application/octet-stream".to_string()
    };

    Ok(([(axum::http::header::CONTENT_TYPE, content_type)], data))
}

// ── Static pages ──────────────────────────────────────────────────────

async fn index_page() -> Html<&'static str> {
    Html(include_str!("../static/chat.html"))
}

async fn manifest_handler() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "application/manifest+json")],
        include_str!("../static/manifest.json"),
    )
}

async fn sw_handler() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "application/javascript")],
        include_str!("../static/sw.js"),
    )
}

// ── Helpers ───────────────────────────────────────────────────────────

fn build_fallback_client(cfg: &sam_core::LlmConfig) -> Option<Arc<dyn LlmBackend>> {
    let key = load_api_key(cfg).ok()?;
    match cfg.provider.as_str() {
        "xai" => XaiClient::new(key, cfg).ok().map(|c| Arc::new(c) as _),
        "openai-compatible" => {
            OpenAiCompatibleClient::new(key, cfg)
                .ok()
                .map(|c| Arc::new(c) as _)
        }
        _ => SamClaudeClient::new(key, cfg).ok().map(|c| Arc::new(c) as _),
    }
}

/// Extract __ATTACHMENT__ markers from LLM reply text.
fn extract_attachments(reply: &str) -> (String, Vec<AttachmentInfo>) {
    let mut attachments = Vec::new();
    let mut clean_lines = Vec::new();
    for line in reply.lines() {
        if let Some(path) = line.strip_prefix("__ATTACHMENT__:") {
            let path_obj = std::path::Path::new(path);
            let filename = path_obj
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_else(|| "file".to_string());
            let mime = mime_from_ext(path);
            let id = uuid::Uuid::new_v4().to_string();

            let uploads_dir = sam_core::state_dir().join("uploads");
            let _ = std::fs::create_dir_all(&uploads_dir);
            let dest = uploads_dir.join(&id);
            if let Err(e) = std::fs::copy(path, &dest) {
                warn!(error = %e, path = path, "failed to copy attachment");
            }
            let meta_path = uploads_dir.join(format!("{id}.json"));
            let meta = serde_json::json!({ "filename": filename, "mime_type": mime, "original_path": path });
            let _ = std::fs::write(&meta_path, meta.to_string());

            attachments.push(AttachmentInfo { id, filename, mime_type: mime });
        } else {
            clean_lines.push(line);
        }
    }
    (clean_lines.join("\n"), attachments)
}

fn mime_from_ext(path: &str) -> String {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        "m4a" | "aac" => "audio/mp4".to_string(),
        "mp3" => "audio/mpeg".to_string(),
        "wav" => "audio/wav".to_string(),
        "caf" => "audio/x-caf".to_string(),
        "png" => "image/png".to_string(),
        "jpg" | "jpeg" => "image/jpeg".to_string(),
        "gif" => "image/gif".to_string(),
        "webp" => "image/webp".to_string(),
        "pdf" => "application/pdf".to_string(),
        "md" => "text/markdown".to_string(),
        "txt" => "text/plain".to_string(),
        _ => "application/octet-stream".to_string(),
    }
}
