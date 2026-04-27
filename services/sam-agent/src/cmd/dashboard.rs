//! `sam dashboard` — Full-featured web management UI.
//!
//! Provides HTTP dashboard with real-time stats, session management,
//! plugin marketplace, agent routing, and system health monitoring.

use axum::{
    response::Html,
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;
use tracing::info;

use sam_claude::TokenBudget;
use sam_core::{
    config_path, load_config, AgentStore, CronStore, FlowStore, PluginStore, Registry, SkillStore,
};

use chrono::TimeZone;

// ── API Types ──────────────────────────────────────────────────────────

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
    handle: String,
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
struct AgentInfo {
    name: String,
    description: String,
    triggers: Vec<String>,
    priority: i32,
    can_handoff_to: Vec<String>,
}

#[derive(Serialize)]
struct PluginInfo {
    name: String,
    version: String,
    description: String,
    author: String,
    enabled: bool,
    tools_count: usize,
    agents_count: usize,
    source: Option<String>,
}

#[derive(Serialize)]
struct SkillInfo {
    name: String,
    description: String,
}

#[derive(Serialize)]
struct SessionInfo {
    handle: String,
    message_count: usize,
    last_modified: String,
}

#[derive(Serialize)]
struct DashboardData {
    health: HealthStatus,
    budget: BudgetStatus,
    reminders: Vec<ReminderInfo>,
    flows: Vec<FlowInfo>,
    runtime: RuntimeStats,
    agents: Vec<AgentInfo>,
    plugins: Vec<PluginInfo>,
    skills: Vec<SkillInfo>,
    sessions: Vec<SessionInfo>,
}

// ── Routes ────────────────────────────────────────────────────────────

pub async fn run(port: u16) -> i32 {
    let app = Router::new()
        .route("/", get(dashboard_html))
        .route("/api/health", get(api_health))
        .route("/api/dashboard", get(api_dashboard))
        .route("/api/agents", get(api_agents))
        .route("/api/plugins", get(api_plugins))
        .route("/api/skills", get(api_skills))
        .route("/api/sessions", get(api_sessions))
        .route("/api/registry", get(api_registry))
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

// ── API Handlers ──────────────────────────────────────────────────────

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
            handle: j.handle.clone(),
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
    let agents = load_agents();
    let plugins = load_plugins();
    let skills = load_skills();
    let sessions = load_sessions();

    Json(DashboardData {
        health: HealthStatus {
            status: if runtime.uptime_secs > 0 { "ok" } else { "unknown" },
            uptime_secs: runtime.uptime_secs,
            version: env!("CARGO_PKG_VERSION"),
            provider: config.llm.provider.clone(),
            model: config.llm.model.clone(),
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
        agents,
        plugins,
        skills,
        sessions,
    })
}

async fn api_agents() -> Json<Vec<AgentInfo>> {
    Json(load_agents())
}

async fn api_plugins() -> Json<Vec<PluginInfo>> {
    Json(load_plugins())
}

async fn api_skills() -> Json<Vec<SkillInfo>> {
    Json(load_skills())
}

async fn api_sessions() -> Json<Vec<SessionInfo>> {
    Json(load_sessions())
}

async fn api_registry() -> Json<serde_json::Value> {
    match Registry::load_cache() {
        Some(reg) => Json(serde_json::to_value(reg).unwrap_or_default()),
        None => Json(serde_json::json!({"error": "no registry cache"})),
    }
}

async fn dashboard_html() -> Html<&'static str> {
    Html(include_str!("dashboard.html"))
}

// ── Data Loaders ──────────────────────────────────────────────────────

fn load_runtime_stats() -> RuntimeStats {
    let path = sam_core::state_dir().join("daemon_stats.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn load_agents() -> Vec<AgentInfo> {
    let store = AgentStore::load();
    store
        .list()
        .into_iter()
        .map(|a| AgentInfo {
            name: a.name.clone(),
            description: a.description.clone(),
            triggers: a.triggers.clone(),
            priority: a.priority,
            can_handoff_to: a.can_handoff_to.clone(),
        })
        .collect()
}

fn load_plugins() -> Vec<PluginInfo> {
    let store = PluginStore::load();
    store
        .list()
        .into_iter()
        .map(|p| {
            let source = p
                .path
                .join(".sam_source.json")
                .exists()
                .then(|| {
                    std::fs::read_to_string(p.path.join(".sam_source.json"))
                        .ok()
                        .and_then(|d| serde_json::from_str::<serde_json::Value>(&d).ok())
                        .and_then(|v| v["repo"].as_str().map(String::from))
                })
                .flatten();
            PluginInfo {
                name: p.manifest.name.clone(),
                version: p.manifest.version.clone(),
                description: p.manifest.description.clone(),
                author: p.manifest.author.clone(),
                enabled: p.manifest.enabled,
                tools_count: p.tools.len(),
                agents_count: p.agents.len(),
                source,
            }
        })
        .collect()
}

fn load_skills() -> Vec<SkillInfo> {
    let store = SkillStore::load();
    store
        .list()
        .iter()
        .map(|s| SkillInfo {
            name: s.name.clone(),
            description: s.description.clone(),
        })
        .collect()
}

fn load_sessions() -> Vec<SessionInfo> {
    let sessions_dir = sam_core::state_dir().join("sessions");
    let mut sessions = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let handle = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();

            let (msg_count, last_mod) = match std::fs::read_to_string(&path) {
                Ok(data) => {
                    let count = data.matches("\"role\"").count() / 2; // rough estimate
                    let modified = std::fs::metadata(&path)
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .map(|t| {
                            let secs = t
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs() as i64;
                            chrono::Local
                                .timestamp_opt(secs, 0)
                                .single()
                                .map(|dt| dt.format("%m-%d %H:%M").to_string())
                                .unwrap_or_default()
                        })
                        .unwrap_or_default();
                    (count, modified)
                }
                Err(_) => (0, String::new()),
            };

            sessions.push(SessionInfo {
                handle,
                message_count: msg_count,
                last_modified: last_mod,
            });
        }
    }

    sessions.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));
    sessions
}
