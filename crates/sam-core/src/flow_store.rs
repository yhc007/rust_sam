//! Flow store — loads flow definitions from `~/.sam/flows/*.toml`.

use std::path::PathBuf;

use tracing::{debug, info, warn};

use crate::cron_store::cron_matches;
use crate::flow::{FlowDef, FlowTrigger};
use crate::paths::sam_home;

/// Manages all loaded flow definitions.
pub struct FlowStore {
    flows: Vec<FlowDef>,
    dir: PathBuf,
}

impl FlowStore {
    /// Load all `.toml` files from the flows directory.
    pub fn load() -> Self {
        let dir = flows_dir();
        let flows = load_all_flows(&dir);
        info!(count = flows.len(), dir = %dir.display(), "FlowStore loaded");
        Self { flows, dir }
    }

    /// Reload flows from disk.
    pub fn reload(&mut self) {
        self.flows = load_all_flows(&self.dir);
        info!(count = self.flows.len(), "FlowStore reloaded");
    }

    /// Get all loaded flows.
    pub fn list(&self) -> &[FlowDef] {
        &self.flows
    }

    /// Find a flow by name (case-insensitive).
    pub fn get(&self, name: &str) -> Option<&FlowDef> {
        let lower = name.to_lowercase();
        self.flows.iter().find(|f| f.name.to_lowercase() == lower)
    }

    /// Return flows whose cron trigger matches the current time.
    pub fn due_flows(&self, now_unix: i64) -> Vec<&FlowDef> {
        self.flows
            .iter()
            .filter(|f| match &f.trigger {
                FlowTrigger::Cron { expr } => cron_matches(expr, now_unix),
                FlowTrigger::Manual => false,
            })
            .collect()
    }
}

/// Canonical flows directory.
pub fn flows_dir() -> PathBuf {
    sam_home().join("flows")
}

/// Load all TOML flow files from a directory.
fn load_all_flows(dir: &PathBuf) -> Vec<FlowDef> {
    if !dir.exists() {
        debug!(dir = %dir.display(), "flows directory does not exist, skipping");
        return Vec::new();
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "failed to read flows directory");
            return Vec::new();
        }
    };

    let mut flows = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "toml") {
            continue;
        }
        match load_flow_file(&path) {
            Ok(flow) => {
                debug!(name = %flow.name, "loaded flow");
                flows.push(flow);
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to parse flow file");
            }
        }
    }

    flows
}

/// Parse a single flow TOML file.
fn load_flow_file(path: &std::path::Path) -> Result<FlowDef, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("read error: {e}"))?;

    let mut flow: FlowDef = toml::from_str(&raw)
        .map_err(|e| format!("parse error: {e}"))?;

    // Use filename (without extension) as the flow name if not set.
    if flow.name.is_empty() {
        flow.name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unnamed".to_string());
    }

    Ok(flow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_flow_toml() {
        let toml_str = r#"
name = "daily_summary"
description = "매일 아침 요약 생성"

[trigger]
type = "cron"
expr = "0 9 * * *"

[[steps]]
type = "llm"
name = "summarize"
prompt = "오늘의 할 일을 요약해줘."

[[steps]]
type = "send"
name = "deliver"
handle = "+821038600983"
body = "{{summarize.output}}"
"#;
        let flow: FlowDef = toml::from_str(toml_str).expect("parse");
        assert_eq!(flow.name, "daily_summary");
        assert_eq!(flow.steps.len(), 2);
        assert!(matches!(flow.trigger, FlowTrigger::Cron { .. }));
    }

    #[test]
    fn parse_tool_step() {
        let toml_str = r#"
name = "check_weather"
description = "날씨 확인"

[[steps]]
type = "tool"
name = "search"
tool = "web_search"

[steps.input]
query = "서울 날씨"
"#;
        let flow: FlowDef = toml::from_str(toml_str).expect("parse");
        assert_eq!(flow.steps.len(), 1);
    }
}
