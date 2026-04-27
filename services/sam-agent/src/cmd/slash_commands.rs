//! In-chat slash commands — `/help`, `/skills`, `/status`, `/reload`, etc.
//!
//! These are handled locally without calling the LLM, providing instant
//! responses to meta/administrative queries.

use std::sync::Arc;
use tokio::sync::Mutex;

use chrono::TimeZone;
use sam_core::{CronStore, FlowStore, PluginStore, SamConfig, SkillStore};
use sam_claude::TokenBudget;

/// Result of attempting to handle a slash command.
pub enum SlashResult {
    /// The message was a slash command; contains the response text.
    Handled(String),
    /// Not a slash command — pass through to LLM.
    NotACommand,
}

/// Try to handle the incoming text as a slash command.
/// Returns `SlashResult::Handled` with the response if it was a command,
/// or `SlashResult::NotACommand` if it should go to the LLM.
pub async fn try_handle(
    text: &str,
    config: &SamConfig,
    budget: &TokenBudget,
    cron_store: Option<&Arc<Mutex<CronStore>>>,
    flow_store: Option<&Arc<Mutex<FlowStore>>>,
    skill_store: Option<&Arc<Mutex<SkillStore>>>,
    plugin_store: Option<&Arc<Mutex<PluginStore>>>,
) -> SlashResult {
    let trimmed = text.trim();

    // Must start with '/'
    if !trimmed.starts_with('/') {
        return SlashResult::NotACommand;
    }

    let parts: Vec<&str> = trimmed.splitn(2, ' ').collect();
    let cmd = parts[0].to_lowercase();
    let arg = parts.get(1).copied().unwrap_or("");

    match cmd.as_str() {
        "/help" | "/도움" => SlashResult::Handled(cmd_help()),
        "/skills" | "/스킬" => SlashResult::Handled(cmd_skills(skill_store).await),
        "/status" | "/상태" => SlashResult::Handled(cmd_status(config, budget)),
        "/reminders" | "/리마인더" => SlashResult::Handled(cmd_reminders(cron_store).await),
        "/flows" | "/플로우" => SlashResult::Handled(cmd_flows(flow_store).await),
        "/reload" | "/리로드" => SlashResult::Handled(cmd_reload(skill_store).await),
        "/budget" | "/예산" => SlashResult::Handled(cmd_budget(budget)),
        "/clear" | "/초기화" => SlashResult::Handled(cmd_clear()),
        "/version" | "/���전" => SlashResult::Handled(cmd_version()),
        "/skill" => SlashResult::Handled(cmd_skill_detail(arg, skill_store).await),
        "/agent" | "/에이전트" => SlashResult::Handled(cmd_agent(arg).await),
        "/agents" | "/에이전트목록" => SlashResult::Handled(cmd_agents_list().await),
        "/plugins" | "/플러그인" => SlashResult::Handled(cmd_plugins(plugin_store).await),
        "/plugin" => SlashResult::Handled(cmd_plugin_action(arg, plugin_store).await),
        _ => {
            // Unknown slash command — don't swallow, might be user text
            // Only swallow commands that look intentional (single word)
            if trimmed.len() < 20 && !trimmed.contains(' ') {
                SlashResult::Handled(format!(
                    "모르는 명령어: {cmd}\n\n/help 로 사용 가능한 명령어를 확인해봐."
                ))
            } else {
                SlashResult::NotACommand
            }
        }
    }
}

// ── Command implementations ────────────────────────────────────────────

fn cmd_help() -> String {
    r#"📋 Sam 명령어 목록

대화 중 언제든 사용 가능:

/help — 이 도��말
/skills — 사�� 가능한 스킬(도구) 목록
/skill <이름> — 특정 스킬 상세 정보
/status — 현재 상태 (모델, 연결 등)
/budget — 오늘 토큰 사용량
/reminders — 예약된 리마인더 목록
/flows — 등록된 플로우 목록
/reload — 스킬/설정 다��� 로드
/clear — 대화 기록 초기화 (다음 메시지부터)
/version — Sam 버전 정보

한국어도 가능:
/도움 /스킬 /상태 /예산 /리마인더 /플���우 /리로드 /초기화 /버전"#
        .to_string()
}

async fn cmd_skills(skill_store: Option<&Arc<Mutex<SkillStore>>>) -> String {
    let mut output = String::from("🛠️ 사용 가능한 스킬\n\n");

    // Built-in tools
    output.push_str("【내장 도구】\n");
    output.push_str("• memory_recall — 기억 검색\n");
    output.push_str("• memory_store — ��억 저장\n");
    output.push_str("• current_time — 현재 시간\n");
    output.push_str("• run_command — 명령 실행\n");
    output.push_str("• read_file / write_file — 파일 읽기/쓰���\n");
    output.push_str("• claude_code — 코딩 작업\n");
    output.push_str("• web_search — 웹 검색\n");
    output.push_str("• twitter_search — 트위터 검색\n");
    output.push_str("• schedule_reminder — 리마인더 예���\n");
    output.push_str("• run_flow / list_flows — 플로우\n");
    output.push_str("• notion_create_page — 노션 페이지\n");

    // Custom skills
    if let Some(store_arc) = skill_store {
        let store = store_arc.lock().await;
        let skills = store.list();
        if !skills.is_empty() {
            output.push_str(&format!("\n【커스텀 스킬】 ({}개)\n", skills.len()));

            // Group by category (infer from name/command patterns)
            let mut media = Vec::new();
            let mut productivity = Vec::new();
            let mut utility = Vec::new();
            let mut other = Vec::new();

            for s in skills {
                let name = s.name.as_str();
                if matches!(name, "youtube_search" | "music_control" | "tts_speak" | "image_resize" | "qr_generate") {
                    media.push(s);
                } else if matches!(name, "timer" | "clipboard" | "screenshot" | "open_app") {
                    productivity.push(s);
                } else if matches!(name, "calculator" | "translate" | "system_info") {
                    utility.push(s);
                } else {
                    other.push(s);
                }
            }

            if !media.is_empty() {
                output.push_str("  미디어:\n");
                for s in &media {
                    output.push_str(&format!("  • {} — {}\n", s.name, s.description));
                }
            }
            if !productivity.is_empty() {
                output.push_str("  생산성:\n");
                for s in &productivity {
                    output.push_str(&format!("  • {} — {}\n", s.name, s.description));
                }
            }
            if !utility.is_empty() {
                output.push_str("  유틸리티:\n");
                for s in &utility {
                    output.push_str(&format!("  • {} — {}\n", s.name, s.description));
                }
            }
            if !other.is_empty() {
                output.push_str("  기타:\n");
                for s in &other {
                    output.push_str(&format!("  • {} — {}\n", s.name, s.description));
                }
            }
        }
    }

    output.push_str("\n💡 /skill <이름> 으로 상세 정보 확인");
    output
}

fn cmd_status(config: &SamConfig, budget: &TokenBudget) -> String {
    let remaining = budget.remaining();
    let daily_limit = budget.daily_limit;
    let used = daily_limit.saturating_sub(remaining);
    let usage_pct = if daily_limit > 0 {
        (used as f64 / daily_limit as f64 * 100.0) as u32
    } else {
        0
    };

    format!(
        "📊 Sam 상태\n\n\
         모델: {}\n\
         Provider: {}\n\
         토큰: {} / {} ({}% 사용)\n\
         Whisper: {}\n\
         Heartbeat: {}",
        config.llm.model,
        config.llm.provider,
        format_tokens(used),
        format_tokens(daily_limit),
        usage_pct,
        if config.whisper.enabled { "켜짐" } else { "꺼짐" },
        if config.heartbeat.enabled { "켜짐" } else { "꺼짐" },
    )
}

fn cmd_budget(budget: &TokenBudget) -> String {
    let remaining = budget.remaining();
    let daily = budget.daily_limit;
    let used = daily.saturating_sub(remaining);

    format!(
        "💰 토큰 예산\n\n\
         일일 한도: {}\n\
         오늘 사용: {}\n\
         남은 양: {} ({}%)",
        format_tokens(daily),
        format_tokens(used),
        format_tokens(remaining),
        if daily > 0 { remaining * 100 / daily } else { 100 },
    )
}

async fn cmd_reminders(cron_store: Option<&Arc<Mutex<CronStore>>>) -> String {
    let Some(store_arc) = cron_store else {
        return "리마인더 시스템이 비활��화되어 있어.".to_string();
    };
    let store = store_arc.lock().await;
    let jobs = store.list();

    if jobs.is_empty() {
        return "예약��� 리마인더가 없어.".to_string();
    }

    let mut output = format!("⏰ 리마인더 {}건\n\n", jobs.len());
    for job in jobs {
        let sched = match &job.schedule {
            sam_core::CronSchedule::Cron { expr } => format!("🔁 {expr}"),
            sam_core::CronSchedule::Once { at_unix } => {
                let dt = chrono::Local::now()
                    .timezone()
                    .timestamp_opt(*at_unix, 0)
                    .single()
                    .map(|t| t.format("%m/%d %H:%M").to_string())
                    .unwrap_or_else(|| at_unix.to_string());
                format!("📅 {dt}")
            }
        };
        output.push_str(&format!("• {} ({})\n", job.message, sched));
    }
    output
}

async fn cmd_flows(flow_store: Option<&Arc<Mutex<FlowStore>>>) -> String {
    let Some(store_arc) = flow_store else {
        return "플로우 시스템이 비활성화되어 있어.".to_string();
    };
    let store = store_arc.lock().await;
    let flows = store.list();

    if flows.is_empty() {
        return "등��된 플로우가 없어. ~/.sam/flows/ 에 TOML을 추가해봐.".to_string();
    }

    let mut output = format!("🔄 플로우 {}건\n\n", flows.len());
    for flow in flows {
        let trigger = match &flow.trigger {
            sam_core::FlowTrigger::Manual => "수동".to_string(),
            sam_core::FlowTrigger::Cron { expr } => format!("🔁 {expr}"),
        };
        output.push_str(&format!("• {} — {} ({})\n", flow.name, flow.description, trigger));
    }
    output
}

async fn cmd_reload(skill_store: Option<&Arc<Mutex<SkillStore>>>) -> String {
    if let Some(store_arc) = skill_store {
        let mut store = store_arc.lock().await;
        store.reload();
        let count = store.list().len();
        format!("🔄 리로드 완료! 스킬 {count}개 로드됨.")
    } else {
        "스킬 스토어를 찾을 수 없��.".to_string()
    }
}

/// Flag for session clearing — the actual clearing happens in the router.
fn cmd_clear() -> String {
    "🗑️ 대화 기록을 초기화했어. 다음 메���지부터 새 대화로 시작할게.".to_string()
}

fn cmd_version() -> String {
    format!(
        "🤖 Sam v{}\n빌드: {} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_NAME"),
        std::env::consts::ARCH,
    )
}

async fn cmd_agent(name: &str) -> String {
    if name.is_empty() {
        return "사용법: /agent <이름>\n사용 가능: coder, researcher, scheduler, default\n\n/agents 로 전체 목록 확인".to_string();
    }

    let agent_store = sam_core::AgentStore::load();
    if agent_store.get(name).is_some() || name == "default" {
        let desc = agent_store.get(name)
            .map(|a| a.description.as_str())
            .unwrap_or("범용 에이전트 (모든 도구 사용 가능)");
        // Return sentinel that daemon intercepts to switch agent.
        format!("__AGENT_SWITCH__:{}\n🔀 에이전트 전환: {} ({})", name, name, desc)
    } else {
        format!("'{}' 에이전트를 찾을 수 없어. /agents 로 목록 확인.", name)
    }
}

async fn cmd_agents_list() -> String {
    let agent_store = sam_core::AgentStore::load();
    let agents = agent_store.list();

    if agents.is_empty() {
        return "등록된 에이전트가 없어.\n~/.sam/agents/ 에 TOML 파일을 추가해봐.\n\n현재는 단일 에이전트(default) 모드로 동작 중.".to_string();
    }

    let mut output = format!("🤖 에이전트 {}개\n\n", agents.len());
    for agent in &agents {
        output.push_str(&format!(
            "• {} — {}\n  트리거: {}\n",
            agent.name,
            agent.description,
            agent.triggers.join(", "),
        ));
    }
    output.push_str("\n/agent <이름> 으로 전환 가능");
    output
}

async fn cmd_skill_detail(name: &str, skill_store: Option<&Arc<Mutex<SkillStore>>>) -> String {
    if name.is_empty() {
        return "사용법: /skill <스킬이름>\n예: /skill calculator".to_string();
    }

    if let Some(store_arc) = skill_store {
        let store = store_arc.lock().await;
        if let Some(skill) = store.get(name) {
            let mut output = format!("📦 {}\n\n", skill.name);
            output.push_str(&format!("{}\n\n", skill.description));

            // Show parameters
            if let Some(props) = skill.input_schema["properties"].as_object() {
                output.push_str("파라미터:\n");
                let required: Vec<String> = skill.input_schema["required"]
                    .as_array()
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                for (key, val) in props {
                    let desc = val["description"].as_str().unwrap_or("");
                    let req = if required.contains(key) { " (필수)" } else { "" };
                    output.push_str(&format!("  • {key}{req} — {desc}\n"));
                }
            }

            output.push_str(&format!("\n실행: {}", skill.exec.command));
            return output;
        }
    }

    // Check built-in tools
    let builtins = [
        ("memory_recall", "장기 기억에서 관련 내용을 검색"),
        ("memory_store", "중요한 정보를 장기 ��억에 저장"),
        ("current_time", "현재 날짜와 시간을 반환"),
        ("run_command", "쉘 명령을 실행"),
        ("read_file", "파일 내용을 읽기"),
        ("write_file", "파일에 내용을 쓰기"),
        ("claude_code", "복잡한 코딩 작업 수행"),
        ("twitter_search", "트위터에서 검색"),
        ("web_search", "인터넷에서 정�� 검색"),
        ("schedule_reminder", "리마인더 예��"),
        ("list_reminders", "리마인더 목록"),
        ("cancel_reminder", "리마인더 취소"),
        ("run_flow", "플로우 실행"),
        ("list_flows", "플로우 목록"),
        ("notion_create_page", "Notion ���이지 생성"),
    ];

    if let Some((_, desc)) = builtins.iter().find(|(n, _)| *n == name) {
        return format!("📦 {} (내장)\n\n{}", name, desc);
    }

    format!("'{}' 스킬을 ��을 수 없어. /skills 로 목록 확인해봐.", name)
}

// ── Plugin commands ──────────────────────────────────────────────────────

async fn cmd_plugins(plugin_store: Option<&Arc<Mutex<PluginStore>>>) -> String {
    let store = match plugin_store {
        Some(s) => s.lock().await,
        None => return "플러그인 시스템이 비활성화되어 있어.".to_string(),
    };

    let plugins = store.list();
    if plugins.is_empty() {
        return "🔌 설치된 플러그인 없음\n\n\
             ~/.sam/plugins/ 에 플러그인 디렉토리를 추가하면 자동으로 로드돼.\n\
             /plugin help 로 자세한 사용법 확인.".to_string();
    }

    let mut output = format!("🔌 플러그인 목록 ({}/{}개 활성)\n\n",
        store.enabled().len(), plugins.len());

    for p in &plugins {
        let status = if p.manifest.enabled { "✅" } else { "⏸️" };
        output.push_str(&format!(
            "{status} {} v{} — {}\n   도구: {}개, 에이전트: {}개\n",
            p.manifest.name,
            p.manifest.version,
            p.manifest.description,
            p.tools.len(),
            p.agents.len(),
        ));

        let missing = p.check_requirements();
        if !missing.is_empty() {
            output.push_str(&format!("   ⚠️ 미충족 요구사항: {}\n", missing.join(", ")));
        }
    }

    output
}

async fn cmd_plugin_action(arg: &str, plugin_store: Option<&Arc<Mutex<PluginStore>>>) -> String {
    let parts: Vec<&str> = arg.splitn(2, ' ').collect();
    let action = parts.first().copied().unwrap_or("help");
    let name = parts.get(1).copied().unwrap_or("");

    match action {
        "enable" => {
            if name.is_empty() {
                return "사용법: /plugin enable <이름>".to_string();
            }
            let store = match plugin_store {
                Some(s) => s,
                None => return "플러그인 시스템 비활성.".to_string(),
            };
            let mut s = store.lock().await;
            if s.set_enabled(name, true) {
                format!("✅ '{}' 플러그인 활성화됨", name)
            } else {
                format!("'{}' 플러그인을 찾을 수 없어.", name)
            }
        }
        "disable" => {
            if name.is_empty() {
                return "사용법: /plugin disable <이름>".to_string();
            }
            let store = match plugin_store {
                Some(s) => s,
                None => return "플러그인 시스템 비활성.".to_string(),
            };
            let mut s = store.lock().await;
            if s.set_enabled(name, false) {
                format!("⏸️ '{}' 플러그인 비활성화됨", name)
            } else {
                format!("'{}' 플러그인을 찾을 수 없어.", name)
            }
        }
        "reload" => {
            let store = match plugin_store {
                Some(s) => s,
                None => return "플러그인 시스템 비활성.".to_string(),
            };
            let mut s = store.lock().await;
            s.reload();
            format!("🔄 플러그인 리로드 완료 ({}개)", s.list().len())
        }
        "info" => {
            if name.is_empty() {
                return "사용법: /plugin info <이름>".to_string();
            }
            let store = match plugin_store {
                Some(s) => s,
                None => return "플러그인 시스템 비활성.".to_string(),
            };
            let s = store.lock().await;
            match s.get(name) {
                Some(p) => {
                    let mut out = format!(
                        "🔌 {} v{}\n{}\n작성자: {}\n상태: {}\n경로: {}\n\n",
                        p.manifest.name,
                        p.manifest.version,
                        p.manifest.description,
                        p.manifest.author,
                        if p.manifest.enabled { "활성" } else { "비활성" },
                        p.path.display(),
                    );
                    if !p.tools.is_empty() {
                        out.push_str("도구:\n");
                        for t in &p.tools {
                            out.push_str(&format!("  • {} — {}\n", t.name, t.description));
                        }
                    }
                    if !p.agents.is_empty() {
                        out.push_str("에이전트:\n");
                        for a in &p.agents {
                            out.push_str(&format!("  • {} — {}\n", a.name, a.description));
                        }
                    }
                    out
                }
                None => format!("'{}' 플러그인을 찾을 수 없어.", name),
            }
        }
        _ => {
            "🔌 플러그인 명령어\n\n\
             /plugins — 설치된 플러그인 목록\n\
             /plugin info <이름> — 상세 정보\n\
             /plugin enable <이름> — 활성화\n\
             /plugin disable <이름> — 비활성화\n\
             /plugin reload — 전체 리로드"
                .to_string()
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        format!("{tokens}")
    }
}

