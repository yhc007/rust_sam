//! `sam status` — static configuration + live health probes.
//!
//! Exit codes:
//!   0  everything OK
//!   1  one or more WARN probes (functional but degraded)
//!   2  at least one BLOCK probe (cannot run)

use std::path::Path;
use std::process::Command;

use colored::Colorize;
use serde::Serialize;

use sam_core::{
    config_path, load_config, sam_home, tools_dir, SamConfig,
};
use sam_imessage::probe::{automation_status, can_read_chat_db};
use sam_memory_adapter::MemoryAdapter;
use sam_tools::ToolRegistry;

/// Severity of an individual probe.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Severity {
    Ok,
    Warn,
    Block,
}

impl Severity {
    fn exit_code(self) -> i32 {
        match self {
            Self::Ok => 0,
            Self::Warn => 1,
            Self::Block => 2,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "WARN",
            Self::Block => "BLOCK",
        }
    }

    fn colored(self) -> colored::ColoredString {
        match self {
            Self::Ok => "ok".green(),
            Self::Warn => "WARN".yellow(),
            Self::Block => "BLOCK".red(),
        }
    }
}

/// One row of the status report.
#[derive(Debug, Clone, Serialize)]
struct Check {
    name: String,
    severity: Severity,
    detail: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<Check>,
}

impl Check {
    fn new(name: impl Into<String>, severity: Severity, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            severity,
            detail: detail.into(),
            children: Vec::new(),
        }
    }

    fn with_children(mut self, children: Vec<Check>) -> Self {
        self.children = children;
        self
    }

    fn worst(&self) -> Severity {
        let mut w = self.severity;
        for c in &self.children {
            w = std::cmp::max_by_key(w, c.worst(), |s| *s as u8);
        }
        w
    }
}

#[derive(Debug, Serialize)]
struct Report {
    generated_at: String,
    binary: String,
    sam_home: String,
    config_path: String,
    checks: Vec<Check>,
    overall: Severity,
}

pub async fn run(json: bool, verbose: bool) -> i32 {
    let generated_at = chrono::Local::now()
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let sam_home_path = sam_home();
    let config_file = config_path();

    // 1 & 2: static info + config parse.
    let (config_check, loaded_config) = check_config(&config_file);

    // 3: prompts git HEAD.
    let prompts_check = check_prompts(&sam_home_path);

    // 4: iMessage probes.
    let imessage_check = check_imessage();

    // 5: Claude CLI.
    let claude_check = check_claude(loaded_config.as_ref()).await;

    // 6: Memory adapter.
    let memory_check = check_memory(loaded_config.as_ref());

    // 7: Tool registry.
    let tools_check = check_tools(&sam_home_path);

    let binary = format!("sam-agent {}", env!("CARGO_PKG_VERSION"));
    let checks = vec![
        config_check,
        prompts_check,
        imessage_check,
        claude_check,
        memory_check,
        tools_check,
    ];
    let overall = checks.iter().fold(Severity::Ok, |acc, c| {
        std::cmp::max_by_key(acc, c.worst(), |s| *s as u8)
    });

    let report = Report {
        generated_at,
        binary,
        sam_home: sam_home_path.display().to_string(),
        config_path: config_file.display().to_string(),
        checks,
        overall,
    };

    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("failed to serialize status as JSON: {e}");
                return 2;
            }
        }
    } else {
        render_text(&report, verbose);
    }

    report.overall.exit_code()
}

fn check_config(path: &Path) -> (Check, Option<SamConfig>) {
    match load_config(path) {
        Ok(cfg) => {
            let detail = format!(
                "model={}, allowed={}, budget={}, embedder_url={}",
                cfg.llm.model,
                cfg.imessage.allowed_handles.len(),
                cfg.llm.daily_token_budget,
                cfg.memory.embedder_url,
            );
            (Check::new("config", Severity::Ok, detail), Some(cfg))
        }
        Err(e) => (
            Check::new(
                "config",
                Severity::Block,
                format!("unable to load {}: {e}", path.display()),
            ),
            None,
        ),
    }
}

fn check_prompts(sam_home: &Path) -> Check {
    let prompts = sam_home.join("prompts");
    if !prompts.exists() {
        return Check::new(
            "prompts",
            Severity::Warn,
            format!("not found at {}", prompts.display()),
        );
    }
    match Command::new("git")
        .args(["-C"])
        .arg(&prompts)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
    {
        Ok(out) if out.status.success() => {
            let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
            Check::new("prompts", Severity::Ok, format!("HEAD={head}"))
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Check::new(
                "prompts",
                Severity::Warn,
                format!("git rev-parse failed: {stderr}"),
            )
        }
        Err(e) => Check::new("prompts", Severity::Warn, format!("git missing: {e}")),
    }
}

fn check_imessage() -> Check {
    let chat = can_read_chat_db();
    let auto = automation_status();

    let chat_sev = if chat.ok { Severity::Ok } else { Severity::Warn };
    let auto_sev = if auto.ok { Severity::Ok } else { Severity::Warn };

    let poller_state = match sam_imessage::state::load_state() {
        Ok(s) if s.last_seen_rowid > 0 => Check::new(
            "poller_state",
            Severity::Ok,
            format!("last_seen_rowid={}, updated={}", s.last_seen_rowid, s.updated_at),
        ),
        Ok(_) => Check::new("poller_state", Severity::Ok, "no state yet (first run)"),
        Err(e) => Check::new("poller_state", Severity::Warn, format!("load error: {e}")),
    };

    let children = vec![
        Check::new("chat.db", chat_sev, chat.detail),
        Check::new("automation", auto_sev, auto.detail),
        poller_state,
    ];
    let worst = children.iter().fold(Severity::Ok, |acc, c| {
        std::cmp::max_by_key(acc, c.severity, |s| *s as u8)
    });
    Check::new("imessage", worst, "").with_children(children)
}

async fn check_claude(cfg: Option<&SamConfig>) -> Check {
    let Some(cfg) = cfg else {
        return Check::new("claude", Severity::Warn, "skipped (no config)");
    };
    let binary = cfg.claude_code.resolved_binary();
    match sam_claude::probe::claude_version(&binary).await {
        Ok(version) => Check::new("claude", Severity::Ok, version),
        Err(e) => Check::new(
            "claude",
            Severity::Warn,
            format!("{} — {}", binary.display(), e),
        ),
    }
}

fn check_memory(cfg: Option<&SamConfig>) -> Check {
    let mem_config = cfg
        .map(|c| &c.memory)
        .cloned()
        .unwrap_or_default();
    let db_hint = mem_config.resolved_db_path();

    match MemoryAdapter::from_config(&mem_config) {
        Ok(adapter) => {
            let stats = adapter.stats();
            let dream = if stats.dream_active { "on" } else { "off" };
            let detail = format!(
                "mem={}, concepts={}, dream={} (db_hint={})",
                stats.total_memories,
                stats.total_concepts,
                dream,
                db_hint.display(),
            );
            Check::new("memory", Severity::Ok, detail)
        }
        Err(e) => Check::new("memory", Severity::Warn, format!("init failed: {e}")),
    }
}

fn check_tools(sam_home: &Path) -> Check {
    let dir = sam_home.join("tools");
    match ToolRegistry::scan(&dir) {
        Ok(reg) => {
            let detail = format!(
                "registered={} (dir={})",
                reg.count(),
                dir.display()
            );
            Check::new("tools", Severity::Ok, detail)
        }
        Err(e) => Check::new("tools", Severity::Warn, format!("scan error: {e}")),
    }
}

fn render_text(report: &Report, verbose: bool) {
    let sam_home = &report.sam_home;
    println!(
        "{}",
        format!("Sam status — {}", report.generated_at).bold()
    );
    println!("─────────────────────────────────");
    println!("{:<12} {}", "binary", report.binary);
    println!("{:<12} {}", "SAM_HOME", sam_home);
    println!("{:<12} {}", "config", report.config_path);
    println!();

    for check in &report.checks {
        render_check(check, 0, verbose);
    }

    println!("─────────────────────────────────");
    let overall_label = match report.overall {
        Severity::Ok => "OK".green().bold(),
        Severity::Warn => "WARN".yellow().bold(),
        Severity::Block => "BLOCK".red().bold(),
    };
    println!("overall: {overall_label}");
    // Hide `tools_dir()` import warning on unused imports in some configs.
    let _ = tools_dir;
}

fn render_check(check: &Check, indent: usize, verbose: bool) {
    let pad = " ".repeat(indent * 2);
    let label_width = 12usize.saturating_sub(indent * 2).max(6);
    let sev = check.severity.colored();

    if check.children.is_empty() {
        if verbose || !check.detail.is_empty() {
            println!(
                "{pad}{name:<label_width$} {sev}  ({detail})",
                pad = pad,
                name = check.name,
                label_width = label_width,
                sev = sev,
                detail = check.detail,
            );
        } else {
            println!(
                "{pad}{name:<label_width$} {sev}",
                pad = pad,
                name = check.name,
                label_width = label_width,
                sev = sev,
            );
        }
    } else {
        let aggregate = check.worst().colored();
        if check.detail.is_empty() {
            println!(
                "{pad}{name:<label_width$} {sev}",
                pad = pad,
                name = check.name,
                label_width = label_width,
                sev = aggregate,
            );
        } else {
            println!(
                "{pad}{name:<label_width$} {sev}  ({detail})",
                pad = pad,
                name = check.name,
                label_width = label_width,
                sev = aggregate,
                detail = check.detail,
            );
        }
        for child in &check.children {
            render_check(child, indent + 1, verbose);
        }
    }

    let _ = Severity::label;
}
