//! Multi-agent system — declarative agent definitions loaded from `~/.sam/agents/*.toml`.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::paths::sam_home;

/// Declarative agent specification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// Prompt file name (relative to ~/.sam/prompts/), e.g. "coder.md"
    #[serde(default = "AgentDef::default_prompt_file")]
    pub prompt_file: String,
    /// Which tools this agent has access to.
    #[serde(default)]
    pub tools: ToolFilter,
    /// Keyword triggers for auto-routing (case-insensitive).
    #[serde(default)]
    pub triggers: Vec<String>,
    /// Other agent names this agent can hand off to.
    #[serde(default)]
    pub can_handoff_to: Vec<String>,
    /// Priority for routing (higher = checked first). Default: 0.
    #[serde(default)]
    pub priority: i32,
}

impl AgentDef {
    fn default_prompt_file() -> String {
        "system.md".to_string()
    }

    /// Load the system prompt for this agent from ~/.sam/prompts/.
    pub fn load_prompt(&self) -> String {
        let path = sam_home().join("prompts").join(&self.prompt_file);
        match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(_) => {
                warn!(agent = %self.name, path = %path.display(), "agent prompt file not found, using empty");
                String::new()
            }
        }
    }
}

/// Tool access filter for an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum ToolFilter {
    /// Only these named tools are available.
    Allow { names: Vec<String> },
    /// All tools except these.
    Deny { names: Vec<String> },
    /// All tools (default).
    All,
}

impl Default for ToolFilter {
    fn default() -> Self {
        Self::All
    }
}

impl ToolFilter {
    /// Check if a tool name is allowed by this filter.
    pub fn allows(&self, tool_name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Allow { names } => names.iter().any(|n| n == tool_name),
            Self::Deny { names } => !names.iter().any(|n| n == tool_name),
        }
    }
}

/// Store for all loaded agent definitions.
#[derive(Debug, Clone)]
pub struct AgentStore {
    agents: BTreeMap<String, AgentDef>,
}

impl AgentStore {
    /// Scan `~/.sam/agents/*.toml` and parse agent definitions.
    pub fn load() -> Self {
        let dir = agents_dir();
        let agents = load_agents_from_dir(&dir);
        info!(count = agents.len(), dir = %dir.display(), "AgentStore loaded");
        Self { agents }
    }

    /// Create from an existing map (useful for tests).
    pub fn from_map(agents: BTreeMap<String, AgentDef>) -> Self {
        Self { agents }
    }

    /// Get an agent by name.
    pub fn get(&self, name: &str) -> Option<&AgentDef> {
        self.agents.get(name)
    }

    /// List all agents.
    pub fn list(&self) -> Vec<&AgentDef> {
        self.agents.values().collect()
    }

    /// Classify a message to find the best matching agent by keyword triggers.
    /// Returns the agent name if a match is found.
    pub fn classify(&self, text: &str) -> Option<&str> {
        let lower = text.to_lowercase();

        // Sort by priority (descending), check triggers.
        let mut sorted: Vec<&AgentDef> = self.agents.values().collect();
        sorted.sort_by(|a, b| b.priority.cmp(&a.priority));

        for agent in sorted {
            for trigger in &agent.triggers {
                if lower.contains(&trigger.to_lowercase()) {
                    return Some(&agent.name);
                }
            }
        }
        None
    }

    /// Build a classification prompt for LLM-based routing.
    /// Returns (system_prompt, user_prompt) that asks the LLM to pick an agent.
    pub fn build_classify_prompt(&self, user_message: &str, default_agent: &str) -> (String, String) {
        let system = "너는 메시지 분류기야. 사용자 메시지를 보고 가장 적합한 에이전트 이름을 한 단어로만 답해.".to_string();

        let mut agent_list = String::new();
        for agent in self.agents.values() {
            agent_list.push_str(&format!("- {}: {}\n", agent.name, agent.description));
        }

        let user_prompt = format!(
            "사용 가능한 에이전트:\n{agent_list}\n\
             기본 에이전트: {default_agent}\n\n\
             사용자 메시지: \"{user_message}\"\n\n\
             위 메시지를 처리할 가장 적합한 에이전트 이름을 한 단어로만 답해. \
             확실하지 않으면 \"{default_agent}\"라고 답해."
        );

        (system, user_prompt)
    }

    /// Parse LLM classification response — extract a valid agent name.
    pub fn parse_classify_response(&self, response: &str, default_agent: &str) -> String {
        let trimmed = response.trim().to_lowercase();
        // Check if the response matches any known agent name.
        for name in self.agents.keys() {
            if trimmed == name.to_lowercase() || trimmed.contains(&name.to_lowercase()) {
                return name.clone();
            }
        }
        default_agent.to_string()
    }

    /// Re-scan the agents directory.
    pub fn reload(&mut self) {
        let dir = agents_dir();
        self.agents = load_agents_from_dir(&dir);
        info!(count = self.agents.len(), "AgentStore reloaded");
    }

    /// Check if the store is empty (no agents defined).
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }
}

/// Path to `~/.sam/agents/`.
pub fn agents_dir() -> PathBuf {
    sam_home().join("agents")
}

// ── Internal ────────────────────────────────────────────────────────────

fn load_agents_from_dir(dir: &PathBuf) -> BTreeMap<String, AgentDef> {
    let mut agents = BTreeMap::new();

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return agents,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        match load_single_agent(&path) {
            Ok(agent) => {
                info!(name = %agent.name, triggers = ?agent.triggers, "loaded agent");
                agents.insert(agent.name.clone(), agent);
            }
            Err(e) => {
                warn!(path = %path.display(), "failed to parse agent TOML: {e}");
            }
        }
    }

    agents
}

fn load_single_agent(path: &PathBuf) -> Result<AgentDef, String> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("read error: {e}"))?;
    let agent: AgentDef = toml::from_str(&content)
        .map_err(|e| format!("parse error: {e}"))?;
    Ok(agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_filter_all_allows_everything() {
        let filter = ToolFilter::All;
        assert!(filter.allows("any_tool"));
    }

    #[test]
    fn tool_filter_allow_restricts() {
        let filter = ToolFilter::Allow {
            names: vec!["run_command".to_string(), "read_file".to_string()],
        };
        assert!(filter.allows("run_command"));
        assert!(!filter.allows("write_file"));
    }

    #[test]
    fn tool_filter_deny_excludes() {
        let filter = ToolFilter::Deny {
            names: vec!["claude_code".to_string()],
        };
        assert!(filter.allows("run_command"));
        assert!(!filter.allows("claude_code"));
    }

    #[test]
    fn classify_matches_trigger() {
        let mut agents = BTreeMap::new();
        agents.insert("coder".to_string(), AgentDef {
            name: "coder".to_string(),
            description: "code tasks".to_string(),
            prompt_file: "coder.md".to_string(),
            tools: ToolFilter::All,
            triggers: vec!["코드".to_string(), "code".to_string(), "build".to_string()],
            can_handoff_to: vec![],
            priority: 10,
        });
        agents.insert("researcher".to_string(), AgentDef {
            name: "researcher".to_string(),
            description: "research".to_string(),
            prompt_file: "researcher.md".to_string(),
            tools: ToolFilter::All,
            triggers: vec!["검색".to_string(), "찾아".to_string()],
            can_handoff_to: vec![],
            priority: 5,
        });

        let store = AgentStore { agents };
        assert_eq!(store.classify("코드 리뷰 해줘"), Some("coder"));
        assert_eq!(store.classify("이거 검색해줘"), Some("researcher"));
        assert_eq!(store.classify("안녕하세요"), None);
    }

    #[test]
    fn build_classify_prompt_includes_agents() {
        let mut agents = BTreeMap::new();
        agents.insert("coder".to_string(), AgentDef {
            name: "coder".to_string(),
            description: "코딩 작업 처리".to_string(),
            prompt_file: "coder.md".to_string(),
            tools: ToolFilter::All,
            triggers: vec![],
            can_handoff_to: vec![],
            priority: 0,
        });
        agents.insert("scheduler".to_string(), AgentDef {
            name: "scheduler".to_string(),
            description: "일정 관리".to_string(),
            prompt_file: "scheduler.md".to_string(),
            tools: ToolFilter::All,
            triggers: vec![],
            can_handoff_to: vec![],
            priority: 0,
        });
        let store = AgentStore { agents };
        let (_sys, prompt) = store.build_classify_prompt("내일 회의 잡아줘", "default");
        assert!(prompt.contains("coder"));
        assert!(prompt.contains("scheduler"));
        assert!(prompt.contains("내일 회의 잡아줘"));
    }

    #[test]
    fn parse_classify_response_extracts_name() {
        let mut agents = BTreeMap::new();
        agents.insert("coder".to_string(), AgentDef {
            name: "coder".to_string(),
            description: "code".to_string(),
            prompt_file: "coder.md".to_string(),
            tools: ToolFilter::All,
            triggers: vec![],
            can_handoff_to: vec![],
            priority: 0,
        });
        agents.insert("scheduler".to_string(), AgentDef {
            name: "scheduler".to_string(),
            description: "schedule".to_string(),
            prompt_file: "scheduler.md".to_string(),
            tools: ToolFilter::All,
            triggers: vec![],
            can_handoff_to: vec![],
            priority: 0,
        });
        let store = AgentStore { agents };

        assert_eq!(store.parse_classify_response("scheduler", "default"), "scheduler");
        assert_eq!(store.parse_classify_response("  Coder\n", "default"), "coder");
        assert_eq!(store.parse_classify_response("unknown_thing", "default"), "default");
    }

    #[test]
    fn parse_agent_toml() {
        let toml_str = r#"
name = "coder"
description = "Handles coding tasks"
prompt_file = "coder.md"
triggers = ["code", "build", "코드"]
can_handoff_to = ["default"]
priority = 10

[tools]
mode = "allow"
names = ["run_command", "read_file", "write_file", "claude_code"]
"#;
        let agent: AgentDef = toml::from_str(toml_str).unwrap();
        assert_eq!(agent.name, "coder");
        assert_eq!(agent.priority, 10);
        assert!(agent.tools.allows("run_command"));
        assert!(!agent.tools.allows("twitter_search"));
    }
}
