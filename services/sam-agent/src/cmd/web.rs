//! `sam web` — Web chat interface with HTTP API.

use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::Html,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};

use sam_claude::{
    load_api_key, load_system_prompt, ConversationSession, LlmBackend, OpenAiCompatibleClient,
    SamClaudeClient, TokenBudget,
};
use sam_core::{config_path, load_config, SamConfig};
use sam_memory_adapter::MemoryAdapter;

struct AppState {
    session: ConversationSession,
    client: Arc<dyn LlmBackend>,
    budget: TokenBudget,
    memory: Option<MemoryAdapter>,
    config: SamConfig,
}

#[derive(Deserialize)]
struct ChatRequest {
    message: String,
}

#[derive(Serialize)]
struct ChatResponse {
    reply: String,
}

#[derive(Serialize)]
struct MemoryStats {
    total_memories: usize,
    total_concepts: usize,
    hippocampus_active: bool,
    neocortex_active: bool,
    dream_active: bool,
}

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
    let budget = TokenBudget::load_or_new(config.llm.daily_token_budget);

    let memory: Option<MemoryAdapter> = match MemoryAdapter::from_config(&config.memory) {
        Ok(m) => {
            info!(total_memories = m.stats().total_memories, "Memory system ready");
            Some(m)
        }
        Err(e) => {
            warn!("Memory system unavailable: {e}");
            None
        }
    };

    let session = ConversationSession::new("web", system_prompt, max_history);

    let state = Arc::new(Mutex::new(AppState {
        session,
        client,
        budget,
        memory,
        config,
    }));

    let app = Router::new()
        .route("/", get(index_page))
        .route("/api/chat", post(chat_handler))
        .route("/api/memory", get(memory_handler))
        .route("/api/dream", post(dream_handler))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    info!(addr = %addr, "Sam web server starting");
    eprintln!("Sam web server running at http://localhost:{port}");

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();

    0
}

async fn chat_handler(
    State(state): State<Arc<Mutex<AppState>>>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, StatusCode> {
    let mut app = state.lock().await;

    // Destructure to satisfy the borrow checker — session.reply() needs
    // mutable access to session, budget, and memory simultaneously.
    let AppState {
        ref mut session,
        ref client,
        ref mut budget,
        ref mut memory,
        ref config,
    } = *app;

    let reply = match session
        .reply(
            client.as_ref(),
            budget,
            &req.message,
            memory.as_mut(),
            config,
            None,
        )
        .await
    {
        Ok(text) => text,
        Err(e) => {
            error!("LLM error: {e}");
            format!("Error: {e}")
        }
    };

    Ok(Json(ChatResponse { reply }))
}

async fn memory_handler(
    State(state): State<Arc<Mutex<AppState>>>,
) -> Json<MemoryStats> {
    let app = state.lock().await;
    if let Some(ref mem) = app.memory {
        let s = mem.stats();
        Json(MemoryStats {
            total_memories: s.total_memories,
            total_concepts: s.total_concepts,
            hippocampus_active: s.hippocampus_active,
            neocortex_active: s.neocortex_active,
            dream_active: s.dream_active,
        })
    } else {
        Json(MemoryStats {
            total_memories: 0,
            total_concepts: 0,
            hippocampus_active: false,
            neocortex_active: false,
            dream_active: false,
        })
    }
}

async fn dream_handler(
    State(state): State<Arc<Mutex<AppState>>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut app = state.lock().await;
    if let Some(ref mut mem) = app.memory {
        let result = mem.dream();
        Ok(Json(serde_json::json!({ "result": result })))
    } else {
        Ok(Json(serde_json::json!({ "error": "memory system unavailable" })))
    }
}

async fn index_page() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}
