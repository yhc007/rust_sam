//! `sam dashboard` — Health monitoring web UI.
//!
//! Provides a lightweight HTTP dashboard showing token usage, session status,
//! active reminders, flows, and system health.

use axum::{
    response::Html,
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;
use tracing::info;

use sam_claude::TokenBudget;
use sam_core::{config_path, load_config, CronStore, FlowStore};

#[derive(Serialize)]
struct HealthStatus {
    status: &'static str,
    uptime_secs: u64,
    version: &'static str,
    provider: String,
    model: String,
}

#[derive(Serialize)]
struct BudgetStatus {
    daily_limit: u64,
    used_today: u64,
    remaining: u64,
    percentage_used: f64,
}

#[derive(Serialize)]
struct ReminderInfo {
    id: String,
    message: String,
    schedule: String,
    repeat: bool,
}

#[derive(Serialize)]
struct FlowInfo {
    name: String,
    description: String,
    trigger: String,
    steps: usize,
}

#[derive(Serialize, Deserialize, Default)]
struct RuntimeStats {
    started_at: i64,
    uptime_secs: u64,
    messages_received: u64,
    messages_sent: u64,
    active_sessions: usize,
    last_message_at: Option<i64>,
    errors_total: u64,
    whisper_enabled: bool,
    mcp_servers: usize,
    heartbeat_sent: u64,
}

#[derive(Serialize)]
struct DashboardData {
    health: HealthStatus,
    budget: BudgetStatus,
    reminders: Vec<ReminderInfo>,
    flows: Vec<FlowInfo>,
    runtime: RuntimeStats,
}

pub async fn run(port: u16) -> i32 {
    let app = Router::new()
        .route("/", get(dashboard_html))
        .route("/api/health", get(api_health))
        .route("/api/dashboard", get(api_dashboard))
        .layer(CorsLayer::permissive());

    let addr = format!("0.0.0.0:{port}");
    info!(addr = %addr, "Dashboard server starting");

    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind {addr}: {e}");
            return 1;
        }
    };

    println!("Dashboard running at http://localhost:{port}");

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("Server error: {e}");
        return 1;
    }
    0
}

async fn api_health() -> Json<HealthStatus> {
    let config = load_config(config_path()).unwrap_or_default();
    let runtime = load_runtime_stats();
    Json(HealthStatus {
        status: if runtime.uptime_secs > 0 { "ok" } else { "unknown" },
        uptime_secs: runtime.uptime_secs,
        version: env!("CARGO_PKG_VERSION"),
        provider: config.llm.provider,
        model: config.llm.model,
    })
}

async fn api_dashboard() -> Json<DashboardData> {
    let config = load_config(config_path()).unwrap_or_default();

    // Budget
    let budget = TokenBudget::load_or_new(config.llm.daily_token_budget);
    let remaining = budget.remaining();
    let used = config.llm.daily_token_budget.saturating_sub(remaining as u64);
    let pct = if config.llm.daily_token_budget > 0 {
        (used as f64 / config.llm.daily_token_budget as f64) * 100.0
    } else {
        0.0
    };

    // Reminders
    let cron_store = CronStore::load();
    let reminders: Vec<ReminderInfo> = cron_store
        .list()
        .iter()
        .map(|j| ReminderInfo {
            id: j.id.clone(),
            message: j.message.clone(),
            schedule: match &j.schedule {
                sam_core::CronSchedule::Cron { expr } => format!("cron: {expr}"),
                sam_core::CronSchedule::Once { at_unix } => {
                    chrono::Local
                        .timestamp_opt(*at_unix, 0)
                        .single()
                        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| at_unix.to_string())
                }
            },
            repeat: j.repeat,
        })
        .collect();

    // Flows
    let flow_store = FlowStore::load();
    let flows: Vec<FlowInfo> = flow_store
        .list()
        .iter()
        .map(|f| FlowInfo {
            name: f.name.clone(),
            description: f.description.clone(),
            trigger: match &f.trigger {
                sam_core::FlowTrigger::Manual => "manual".to_string(),
                sam_core::FlowTrigger::Cron { expr } => format!("cron: {expr}"),
            },
            steps: f.steps.len(),
        })
        .collect();

    let runtime = load_runtime_stats();

    Json(DashboardData {
        health: HealthStatus {
            status: if runtime.uptime_secs > 0 { "ok" } else { "unknown" },
            uptime_secs: runtime.uptime_secs,
            version: env!("CARGO_PKG_VERSION"),
            provider: config.llm.provider,
            model: config.llm.model,
        },
        budget: BudgetStatus {
            daily_limit: config.llm.daily_token_budget,
            used_today: used,
            remaining: remaining as u64,
            percentage_used: pct,
        },
        reminders,
        flows,
        runtime,
    })
}

async fn dashboard_html() -> Html<&'static str> {
    Html(include_str!("dashboard.html"))
}

/// Load daemon runtime stats from the shared JSON file.
fn load_runtime_stats() -> RuntimeStats {
    let path = sam_core::state_dir().join("daemon_stats.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

use chrono::TimeZone;
