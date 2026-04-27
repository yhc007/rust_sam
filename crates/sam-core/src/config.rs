//! Sam configuration loader.
//!
//! The config file lives at `~/.sam/config.toml` and is parsed into
//! [`SamConfig`]. Paths inside the config that begin with `~/` are expanded
//! via [`crate::paths::expand_tilde`] when accessed.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::SamError;
use crate::paths::expand_tilde;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SamConfig {
    #[serde(default)]
    pub identity: IdentityConfig,
    #[serde(default)]
    pub imessage: IMessageConfig,
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub claude_code: ClaudeCodeConfig,
    #[serde(default)]
    pub notion: NotionConfig,
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub twitter: TwitterConfig,
    #[serde(default)]
    pub web_search: WebSearchConfig,
    #[serde(default)]
    pub safety: SafetyConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub whisper: WhisperConfig,
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,
    #[serde(default)]
    pub agents: AgentConfig,
    #[serde(default)]
    pub browser: BrowserConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityConfig {
    #[serde(default = "IdentityConfig::default_name")]
    pub name: String,
    #[serde(default = "IdentityConfig::default_owner")]
    pub owner: String,
    #[serde(default = "IdentityConfig::default_locale")]
    pub locale: String,
}

impl IdentityConfig {
    fn default_name() -> String { "Sam".to_string() }
    fn default_owner() -> String { "Paul".to_string() }
    fn default_locale() -> String { "ko_KR".to_string() }
}

impl Default for IdentityConfig {
    fn default() -> Self {
        Self {
            name: Self::default_name(),
            owner: Self::default_owner(),
            locale: Self::default_locale(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IMessageConfig {
    #[serde(default)]
    pub allowed_handles: Vec<String>,
    #[serde(default = "IMessageConfig::default_poll")]
    pub poll_interval_ms: u64,
    #[serde(default = "IMessageConfig::default_send")]
    pub send_rate_limit_ms: u64,
}

impl IMessageConfig {
    fn default_poll() -> u64 { 1000 }
    fn default_send() -> u64 { 300 }
}

impl Default for IMessageConfig {
    fn default() -> Self {
        Self {
            allowed_handles: Vec::new(),
            poll_interval_ms: Self::default_poll(),
            send_rate_limit_ms: Self::default_send(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    #[serde(default = "LlmConfig::default_provider")]
    pub provider: String,
    #[serde(default = "LlmConfig::default_model")]
    pub model: String,
    #[serde(default = "LlmConfig::default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "LlmConfig::default_budget")]
    pub daily_token_budget: u64,
    #[serde(default)]
    pub api_key_source: Option<String>,
    #[serde(default = "LlmConfig::default_base_url")]
    pub base_url: String,
    #[serde(default = "LlmConfig::default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "LlmConfig::default_retries")]
    pub max_retries: u32,
    #[serde(default = "LlmConfig::default_temperature")]
    pub temperature: f32,
    #[serde(default = "LlmConfig::default_history")]
    pub max_history: usize,
    /// Maximum estimated tokens for conversation history before compaction.
    /// History exceeding this is trimmed and summarized. Default: 16000.
    #[serde(default = "LlmConfig::default_max_context_tokens")]
    pub max_context_tokens: usize,
    /// Maximum characters for the rolling context summary. Default: 1200.
    #[serde(default = "LlmConfig::default_max_summary_chars")]
    pub max_summary_chars: usize,
    /// Seconds before sending "..." ack message while waiting for LLM.
    /// 0 = disabled.
    #[serde(default = "LlmConfig::default_ack_delay")]
    pub ack_delay_secs: u64,
}

impl LlmConfig {
    fn default_provider() -> String { "anthropic".to_string() }
    fn default_model() -> String { "claude-sonnet-4-20250514".to_string() }
    fn default_max_tokens() -> u32 { 4096 }
    fn default_budget() -> u64 { 1_000_000 }
    fn default_base_url() -> String { "https://api.anthropic.com".to_string() }
    fn default_timeout() -> u64 { 60 }
    fn default_retries() -> u32 { 3 }
    fn default_temperature() -> f32 { 0.7 }
    fn default_history() -> usize { 20 }
    fn default_max_context_tokens() -> usize { 16_000 }
    fn default_max_summary_chars() -> usize { 1200 }
    fn default_ack_delay() -> u64 { 5 }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: Self::default_provider(),
            model: Self::default_model(),
            max_tokens: Self::default_max_tokens(),
            daily_token_budget: Self::default_budget(),
            api_key_source: None,
            base_url: Self::default_base_url(),
            timeout_secs: Self::default_timeout(),
            max_retries: Self::default_retries(),
            temperature: Self::default_temperature(),
            max_history: Self::default_history(),
            max_context_tokens: Self::default_max_context_tokens(),
            max_summary_chars: Self::default_max_summary_chars(),
            ack_delay_secs: Self::default_ack_delay(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "MemoryConfig::default_db")]
    pub db_path: String,
    #[serde(default = "MemoryConfig::default_embedder")]
    pub embedder: String,
    #[serde(default = "MemoryConfig::default_embedder_url")]
    pub embedder_url: String,
    #[serde(default = "MemoryConfig::default_cron")]
    pub nightly_sleep_cron: String,
}

impl MemoryConfig {
    fn default_db() -> String { "~/.sam/data/brain".to_string() }
    fn default_embedder() -> String { "bge-m3-http".to_string() }
    fn default_embedder_url() -> String { "http://localhost:3200".to_string() }
    fn default_cron() -> String { "30 3 * * *".to_string() }

    /// Resolved, tilde-expanded db path.
    pub fn resolved_db_path(&self) -> PathBuf {
        PathBuf::from(expand_tilde(&self.db_path))
    }
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            db_path: Self::default_db(),
            embedder: Self::default_embedder(),
            embedder_url: Self::default_embedder_url(),
            nightly_sleep_cron: Self::default_cron(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeCodeConfig {
    #[serde(default = "ClaudeCodeConfig::default_binary")]
    pub binary: String,
    #[serde(default = "ClaudeCodeConfig::default_mode")]
    pub default_permission_mode: String,
    #[serde(default = "ClaudeCodeConfig::default_root")]
    pub session_root: String,
    #[serde(default = "ClaudeCodeConfig::default_timeout")]
    pub hard_timeout_secs: u64,
    #[serde(default = "ClaudeCodeConfig::default_cost")]
    pub hard_cost_usd: u64,
    #[serde(default = "ClaudeCodeConfig::default_max_turns")]
    pub max_turns: u32,
}

impl ClaudeCodeConfig {
    fn default_binary() -> String { "/usr/local/bin/claude".to_string() }
    fn default_mode() -> String { "bypassPermissions".to_string() }
    fn default_root() -> String { "~/.sam/sessions".to_string() }
    fn default_timeout() -> u64 { 7200 }
    fn default_cost() -> u64 { 100 }
    fn default_max_turns() -> u32 { 80 }

    /// Resolved binary path (tilde expanded).
    pub fn resolved_binary(&self) -> PathBuf {
        PathBuf::from(expand_tilde(&self.binary))
    }

    /// Resolved session root (tilde expanded).
    pub fn resolved_session_root(&self) -> PathBuf {
        PathBuf::from(expand_tilde(&self.session_root))
    }
}

impl Default for ClaudeCodeConfig {
    fn default() -> Self {
        Self {
            binary: Self::default_binary(),
            default_permission_mode: Self::default_mode(),
            session_root: Self::default_root(),
            hard_timeout_secs: Self::default_timeout(),
            hard_cost_usd: Self::default_cost(),
            max_turns: Self::default_max_turns(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "TelegramConfig::default_token_source")]
    pub bot_token_source: String,
    /// Telegram user IDs allowed to chat with Sam (empty = allow all).
    #[serde(default)]
    pub allowed_user_ids: Vec<i64>,
    #[serde(default = "TelegramConfig::default_poll_timeout")]
    pub poll_timeout_secs: u64,
}

impl TelegramConfig {
    fn default_token_source() -> String {
        "file:~/.sam/telegram_bot_token".to_string()
    }
    fn default_poll_timeout() -> u64 { 30 }
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token_source: Self::default_token_source(),
            allowed_user_ids: Vec::new(),
            poll_timeout_secs: Self::default_poll_timeout(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NotionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub parent_page_id: String,
    #[serde(default)]
    pub api_key_source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwitterConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "TwitterConfig::default_bearer_source")]
    pub bearer_token_source: String,
}

impl TwitterConfig {
    fn default_bearer_source() -> String {
        "file:~/.sam/twitter_bearer".to_string()
    }
}

impl Default for TwitterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bearer_token_source: Self::default_bearer_source(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchConfig {
    #[serde(default)]
    pub enabled: bool,
    /// "xai" (uses xAI/Grok live search) or "brave" (Brave Search API).
    #[serde(default = "WebSearchConfig::default_provider")]
    pub provider: String,
    /// API key source. For xai, reuses [llm].api_key_source by default.
    #[serde(default)]
    pub api_key_source: Option<String>,
    #[serde(default = "WebSearchConfig::default_max_results")]
    pub max_results: u32,
}

impl WebSearchConfig {
    fn default_provider() -> String { "xai".to_string() }
    fn default_max_results() -> u32 { 5 }
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: Self::default_provider(),
            api_key_source: None,
            max_results: Self::default_max_results(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyConfig {
    #[serde(default = "SafetyConfig::default_ttl")]
    pub confirmation_ttl_secs: u64,
    #[serde(default)]
    pub destructive_patterns: Vec<String>,
}

impl SafetyConfig {
    fn default_ttl() -> u64 { 120 }
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            confirmation_ttl_secs: Self::default_ttl(),
            destructive_patterns: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WhisperConfig {
    /// Enable voice memo transcription.
    #[serde(default)]
    pub enabled: bool,
    /// Whisper API URL. Default: http://localhost:8080/v1/audio/transcriptions
    #[serde(default)]
    pub url: Option<String>,
    /// Model name (default: "whisper-1").
    #[serde(default)]
    pub model: Option<String>,
    /// API key source (env: or file:). Optional for local servers.
    #[serde(default)]
    pub api_key_source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatConfig {
    /// Enable proactive autonomous mode.
    #[serde(default = "HeartbeatConfig::default_enabled")]
    pub enabled: bool,
    /// Morning brief hour (0-23). Default: 8.
    #[serde(default = "HeartbeatConfig::default_morning_hour")]
    pub morning_hour: u32,
    /// Evening summary hour (0-23). Default: 21.
    #[serde(default = "HeartbeatConfig::default_evening_hour")]
    pub evening_hour: u32,
    /// Check interval in seconds. Default: 1800 (30 min).
    #[serde(default = "HeartbeatConfig::default_interval_secs")]
    pub interval_secs: u64,
    /// Minutes before a reminder to send a nudge. Default: 30.
    #[serde(default = "HeartbeatConfig::default_nudge_before_mins")]
    pub nudge_before_mins: i64,
    /// Waking hours start. Default: 8.
    #[serde(default = "HeartbeatConfig::default_wake_hour")]
    pub wake_hour: u32,
    /// Waking hours end. Default: 22.
    #[serde(default = "HeartbeatConfig::default_sleep_hour")]
    pub sleep_hour: u32,
}

impl HeartbeatConfig {
    fn default_enabled() -> bool { true }
    fn default_morning_hour() -> u32 { 8 }
    fn default_evening_hour() -> u32 { 21 }
    fn default_interval_secs() -> u64 { 1800 }
    fn default_nudge_before_mins() -> i64 { 30 }
    fn default_wake_hour() -> u32 { 8 }
    fn default_sleep_hour() -> u32 { 22 }
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: Self::default_enabled(),
            morning_hour: Self::default_morning_hour(),
            evening_hour: Self::default_evening_hour(),
            interval_secs: Self::default_interval_secs(),
            nudge_before_mins: Self::default_nudge_before_mins(),
            wake_hour: Self::default_wake_hour(),
            sleep_hour: Self::default_sleep_hour(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Enable multi-agent routing.
    #[serde(default)]
    pub enabled: bool,
    /// Default agent name when no routing match.
    #[serde(default = "AgentConfig::default_agent")]
    pub default_agent: String,
    /// Use LLM-based classification (expensive) vs keyword-only.
    #[serde(default)]
    pub llm_classify: bool,
}

impl AgentConfig {
    fn default_agent() -> String { "default".to_string() }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_agent: Self::default_agent(),
            llm_classify: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserConfig {
    /// Enable browser automation tool.
    #[serde(default)]
    pub enabled: bool,
    /// Path to Chrome/Chromium binary.
    #[serde(default = "BrowserConfig::default_chrome")]
    pub chrome_path: String,
    /// Page load timeout in seconds.
    #[serde(default = "BrowserConfig::default_timeout")]
    pub timeout_secs: u64,
    /// Max content bytes to return from get_content.
    #[serde(default = "BrowserConfig::default_max_content")]
    pub max_content_bytes: usize,
}

impl BrowserConfig {
    fn default_chrome() -> String {
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".to_string()
    }
    fn default_timeout() -> u64 { 30 }
    fn default_max_content() -> usize { 16_000 }
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            chrome_path: Self::default_chrome(),
            timeout_secs: Self::default_timeout(),
            max_content_bytes: Self::default_max_content(),
        }
    }
}

/// Load and parse a [`SamConfig`] from the given path. The path is used in
/// error messages but its content is read verbatim.
pub fn load_config(path: impl AsRef<Path>) -> Result<SamConfig, SamError> {
    let path = path.as_ref();
    let raw = std::fs::read_to_string(path)
        .map_err(|source| SamError::io(path.to_path_buf(), source))?;
    let cfg: SamConfig = toml::from_str(&raw).map_err(|source| SamError::ConfigParse {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[identity]
name = "Sam"
owner = "Paul"
locale = "ko_KR"

[imessage]
allowed_handles = ["+821038600983"]
poll_interval_ms = 500
send_rate_limit_ms = 200

[llm]
provider = "anthropic"
model = "claude-sonnet-4-20250514"
max_tokens = 4096
daily_token_budget = 1000000

[memory]
db_path = "~/.sam/data/brain"
embedder = "bge-m3-http"
embedder_url = "http://localhost:3200"
nightly_sleep_cron = "30 3 * * *"

[claude_code]
binary = "/usr/local/bin/claude"
default_permission_mode = "plan"
session_root = "~/.sam/sessions"
hard_timeout_secs = 7200
hard_cost_usd = 100

[notion]
enabled = false
parent_page_id = ""

[safety]
confirmation_ttl_secs = 120
destructive_patterns = ["rm -rf"]
"#;

    #[test]
    fn parses_full_config() {
        let cfg: SamConfig = toml::from_str(SAMPLE).expect("parse");
        assert_eq!(cfg.identity.name, "Sam");
        assert_eq!(cfg.imessage.allowed_handles, vec!["+821038600983"]);
        assert_eq!(cfg.llm.daily_token_budget, 1_000_000);
        assert_eq!(cfg.memory.embedder_url, "http://localhost:3200");
        assert_eq!(cfg.safety.destructive_patterns.len(), 1);
    }

    #[test]
    fn tilde_expansion_resolves_db_path() {
        let cfg: SamConfig = toml::from_str(SAMPLE).expect("parse");
        let resolved = cfg.memory.resolved_db_path();
        assert!(resolved.is_absolute(), "expected expanded absolute path, got {resolved:?}");
        assert!(resolved.ends_with(".sam/data/brain"));
    }

    #[test]
    fn load_config_round_trip(
        // runs against a temp file
    ) {
        let tmp = std::env::temp_dir().join("sam-core-config-test.toml");
        std::fs::write(&tmp, SAMPLE).unwrap();
        let cfg = load_config(&tmp).expect("load");
        assert_eq!(cfg.identity.owner, "Paul");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn load_config_reports_bad_path() {
        let err = load_config("/no/such/sam-core-nonexistent.toml").unwrap_err();
        matches!(err, SamError::Io { .. });
    }
}
