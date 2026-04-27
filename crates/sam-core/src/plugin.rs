//! Plugin system — packages that bundle tools, agents, prompts, and scripts.
//!
//! ## Directory layout
//!
//! ```text
//! ~/.sam/plugins/
//!   weather/
//!     plugin.toml       # manifest (required)
//!     tools/            # tool definitions (*.toml, same format as ~/.sam/tools/)
//!     agents/           # agent definitions (*.toml)
//!     prompts/          # agent prompt files (*.md)
//!     bin/              # executable scripts
//!   translator/
//!     plugin.toml
//!     ...
//! ```

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::agent::AgentDef;
use crate::paths::sam_home;
use crate::skill_store::CustomSkill;

/// Plugin manifest parsed from `plugin.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    /// External commands this plugin requires on PATH.
    #[serde(default)]
    pub requires: Vec<String>,
    /// Whether this plugin is enabled (default: true).
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// A loaded plugin with its resolved contents.
#[derive(Debug, Clone)]
pub struct Plugin {
    pub manifest: PluginManifest,
    /// Absolute path to the plugin directory.
    pub path: PathBuf,
    /// Tools provided by this plugin.
    pub tools: Vec<CustomSkill>,
    /// Agents provided by this plugin.
    pub agents: Vec<AgentDef>,
}

impl Plugin {
    /// Check if all required external commands are available.
    pub fn check_requirements(&self) -> Vec<String> {
        let mut missing = Vec::new();
        for req in &self.manifest.requires {
            if which(req).is_none() {
                missing.push(req.clone());
            }
        }
        missing
    }

    /// Get the plugin's bin directory (for PATH extension).
    pub fn bin_dir(&self) -> PathBuf {
        self.path.join("bin")
    }
}

/// Registry of all loaded plugins.
#[derive(Debug, Clone)]
pub struct PluginStore {
    plugins: HashMap<String, Plugin>,
}

impl PluginStore {
    /// Scan `~/.sam/plugins/` and load all valid plugins.
    pub fn load() -> Self {
        let dir = plugins_dir();
        let plugins = load_plugins_from_dir(&dir);
        let enabled = plugins.values().filter(|p| p.manifest.enabled).count();
        info!(
            total = plugins.len(),
            enabled = enabled,
            dir = %dir.display(),
            "PluginStore loaded"
        );
        Self { plugins }
    }

    /// Re-scan the plugins directory.
    pub fn reload(&mut self) {
        let dir = plugins_dir();
        self.plugins = load_plugins_from_dir(&dir);
        info!(count = self.plugins.len(), "PluginStore reloaded");
    }

    /// Get a plugin by name.
    pub fn get(&self, name: &str) -> Option<&Plugin> {
        self.plugins.get(name)
    }

    /// List all plugins.
    pub fn list(&self) -> Vec<&Plugin> {
        self.plugins.values().collect()
    }

    /// List only enabled plugins.
    pub fn enabled(&self) -> Vec<&Plugin> {
        self.plugins
            .values()
            .filter(|p| p.manifest.enabled)
            .collect()
    }

    /// Collect all tool definitions from enabled plugins.
    /// Returns (name, description, input_schema) tuples.
    pub fn tool_definitions_raw(&self) -> Vec<(String, String, serde_json::Value)> {
        self.enabled()
            .iter()
            .flat_map(|p| {
                p.tools.iter().map(|t| {
                    (t.name.clone(), t.description.clone(), t.input_schema.clone())
                })
            })
            .collect()
    }

    /// Collect all agent definitions from enabled plugins.
    pub fn agent_definitions(&self) -> Vec<AgentDef> {
        self.enabled()
            .iter()
            .flat_map(|p| p.agents.clone())
            .collect()
    }

    /// Collect all custom skills from enabled plugins (for tool execution dispatch).
    pub fn all_skills(&self) -> Vec<CustomSkill> {
        self.enabled()
            .iter()
            .flat_map(|p| p.tools.clone())
            .collect()
    }

    /// Enable or disable a plugin. Returns true if the plugin was found.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        if let Some(plugin) = self.plugins.get_mut(name) {
            plugin.manifest.enabled = enabled;
            // Persist the change to plugin.toml.
            let toml_path = plugin.path.join("plugin.toml");
            if let Ok(content) = fs::read_to_string(&toml_path) {
                let updated = if enabled {
                    content.replace("enabled = false", "enabled = true")
                } else {
                    // If no explicit enabled field, add one.
                    if content.contains("enabled") {
                        content.replace("enabled = true", "enabled = false")
                    } else {
                        format!("{content}\nenabled = false\n")
                    }
                };
                let _ = fs::write(&toml_path, updated);
            }
            info!(plugin = name, enabled = enabled, "plugin toggled");
            true
        } else {
            false
        }
    }

    /// Check if the store is empty.
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Install a plugin from a directory path (copy into ~/.sam/plugins/).
    pub fn install_from_path(&mut self, source: &Path) -> Result<String, String> {
        if !source.exists() || !source.is_dir() {
            return Err(format!("소스 디렉토리가 존재하지 않습니다: {}", source.display()));
        }

        let manifest_path = source.join("plugin.toml");
        if !manifest_path.exists() {
            return Err("plugin.toml이 없습니다.".to_string());
        }

        let manifest = parse_manifest(&manifest_path)
            .ok_or("plugin.toml 파싱 실패")?;

        let dest = plugins_dir().join(&manifest.name);
        if dest.exists() {
            return Err(format!("플러그인 '{}' 이 이미 설치되어 있습니다.", manifest.name));
        }

        copy_dir_recursive(source, &dest)
            .map_err(|e| format!("복사 실패: {e}"))?;

        // Reload to pick up the new plugin.
        self.reload();
        Ok(format!("플러그인 '{}' v{} 설치 완료", manifest.name, manifest.version))
    }
}

/// Path to `~/.sam/plugins/`.
pub fn plugins_dir() -> PathBuf {
    sam_home().join("plugins")
}

/// Path to the local registry cache `~/.sam/state/registry.json`.
pub fn registry_cache_path() -> PathBuf {
    sam_home().join("state").join("registry.json")
}

// ── Marketplace ─────────────────────────────────────────────────────────

/// Default registry URL (GitHub-hosted JSON index).
pub const DEFAULT_REGISTRY_URL: &str =
    "https://raw.githubusercontent.com/yhc007/sam-plugins/main/registry.json";

/// A plugin entry in the remote registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    /// GitHub repo in "owner/repo" format.
    pub repo: String,
    /// Optional subdirectory within the repo.
    #[serde(default)]
    pub subdir: String,
    /// Tags for search.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Minimum Sam version required (empty = any).
    #[serde(default)]
    pub min_sam_version: String,
}

/// The full registry (list of available plugins).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registry {
    /// Schema version for the registry format.
    #[serde(default = "default_registry_version")]
    pub version: u32,
    pub plugins: Vec<RegistryEntry>,
}

fn default_registry_version() -> u32 {
    1
}

impl Registry {
    /// Load from a local cache file.
    pub fn load_cache() -> Option<Self> {
        let path = registry_cache_path();
        let content = fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Save to local cache.
    pub fn save_cache(&self) {
        let path = registry_cache_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write(path, json);
        }
    }

    /// Search plugins by query (matches name, description, tags).
    pub fn search(&self, query: &str) -> Vec<&RegistryEntry> {
        let q = query.to_lowercase();
        self.plugins
            .iter()
            .filter(|p| {
                p.name.to_lowercase().contains(&q)
                    || p.description.to_lowercase().contains(&q)
                    || p.tags.iter().any(|t| t.to_lowercase().contains(&q))
            })
            .collect()
    }

    /// Find a plugin by exact name.
    pub fn find(&self, name: &str) -> Option<&RegistryEntry> {
        self.plugins.iter().find(|p| p.name == name)
    }
}

impl PluginStore {
    /// Install a plugin from a GitHub repo URL.
    /// Supports formats:
    /// - "owner/repo" (uses the whole repo as plugin)
    /// - "owner/repo/subdir" (specific directory within repo)
    /// - Full URL "https://github.com/owner/repo"
    pub fn install_from_github(&mut self, source: &str) -> Result<String, String> {
        let (owner, repo, subdir) = parse_github_source(source)?;
        let repo_url = format!("https://github.com/{owner}/{repo}.git");

        let plugins_dir = plugins_dir();
        let _ = fs::create_dir_all(&plugins_dir);

        // Determine plugin name from subdir or repo.
        let plugin_name = if subdir.is_empty() {
            repo.clone()
        } else {
            subdir.split('/').next_back().unwrap_or(&repo).to_string()
        };

        let dest = plugins_dir.join(&plugin_name);
        if dest.exists() {
            return Err(format!(
                "플러그인 '{plugin_name}' 이미 설치됨. 업데이트: /plugin update {plugin_name}"
            ));
        }

        // Clone the repo (or download).
        if subdir.is_empty() {
            // Simple clone.
            let output = std::process::Command::new("git")
                .args(["clone", "--depth", "1", &repo_url, &dest.to_string_lossy()])
                .output()
                .map_err(|e| format!("git clone 실행 실패: {e}"))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("git clone 실패: {stderr}"));
            }

            // Remove .git directory to save space.
            let _ = fs::remove_dir_all(dest.join(".git"));
        } else {
            // Clone to temp, copy subdir.
            let tmp = std::env::temp_dir().join(format!("sam_plugin_clone_{repo}"));
            let _ = fs::remove_dir_all(&tmp);

            let output = std::process::Command::new("git")
                .args(["clone", "--depth", "1", &repo_url, &tmp.to_string_lossy()])
                .output()
                .map_err(|e| format!("git clone 실행 실패: {e}"))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let _ = fs::remove_dir_all(&tmp);
                return Err(format!("git clone 실패: {stderr}"));
            }

            let src = tmp.join(&subdir);
            if !src.exists() {
                let _ = fs::remove_dir_all(&tmp);
                return Err(format!("서브디렉토리 '{subdir}' 를 찾을 수 없습니다."));
            }

            copy_dir_recursive(&src, &dest)
                .map_err(|e| format!("복사 실패: {e}"))?;
            let _ = fs::remove_dir_all(&tmp);
        }

        // Verify plugin.toml exists.
        if !dest.join("plugin.toml").exists() {
            let _ = fs::remove_dir_all(&dest);
            return Err("다운로드한 디렉토리에 plugin.toml이 없습니다.".to_string());
        }

        // Store source info for updates.
        let source_info = serde_json::json!({
            "source": source,
            "repo": format!("{owner}/{repo}"),
            "subdir": subdir,
            "installed_at": chrono::Local::now().to_rfc3339(),
        });
        let _ = fs::write(
            dest.join(".sam_source.json"),
            serde_json::to_string_pretty(&source_info).unwrap_or_default(),
        );

        self.reload();

        let msg = match self.get(&plugin_name) {
            Some(p) => format!(
                "✅ 플러그인 '{}' v{} 설치 완료\n도구 {}개, 에이전트 {}개",
                p.manifest.name,
                p.manifest.version,
                p.tools.len(),
                p.agents.len(),
            ),
            None => "⚠️ 설치는 됐지만 로드에 실패했습니다. 로그를 확인하세요.".to_string(),
        };
        Ok(msg)
    }

    /// Uninstall a plugin by name.
    pub fn uninstall(&mut self, name: &str) -> Result<String, String> {
        let dir = plugins_dir().join(name);
        if !dir.exists() {
            return Err(format!("플러그인 '{name}' 을 찾을 수 없습니다."));
        }
        fs::remove_dir_all(&dir)
            .map_err(|e| format!("삭제 실패: {e}"))?;
        self.reload();
        Ok(format!("🗑️ 플러그인 '{name}' 삭제 완료"))
    }

    /// Update a plugin by re-downloading from its source.
    pub fn update(&mut self, name: &str) -> Result<String, String> {
        let dir = plugins_dir().join(name);
        if !dir.exists() {
            return Err(format!("플러그인 '{name}' 을 찾을 수 없습니다."));
        }

        // Read source info.
        let source_path = dir.join(".sam_source.json");
        let source = if source_path.exists() {
            let data = fs::read_to_string(&source_path)
                .map_err(|e| format!("소스 정보 읽기 실패: {e}"))?;
            let info: serde_json::Value = serde_json::from_str(&data)
                .map_err(|_| "소스 정보 파싱 실패".to_string())?;
            info["source"]
                .as_str()
                .unwrap_or("")
                .to_string()
        } else {
            return Err(format!(
                "플러그인 '{name}'의 설치 소스 정보가 없습니다. 수동 재설치가 필요합니다."
            ));
        };

        if source.is_empty() {
            return Err("소스 정보가 비어있습니다.".to_string());
        }

        // Remove old version.
        fs::remove_dir_all(&dir)
            .map_err(|e| format!("기존 버전 삭제 실패: {e}"))?;

        // Re-install.
        self.install_from_github(&source)
    }

    /// List installed plugins with update info.
    pub fn list_with_source(&self) -> Vec<(&Plugin, Option<String>)> {
        self.plugins
            .values()
            .map(|p| {
                let source = p.path.join(".sam_source.json");
                let repo = if source.exists() {
                    fs::read_to_string(&source)
                        .ok()
                        .and_then(|d| serde_json::from_str::<serde_json::Value>(&d).ok())
                        .and_then(|v| v["repo"].as_str().map(String::from))
                } else {
                    None
                };
                (p, repo)
            })
            .collect()
    }
}

/// Parse a GitHub source string into (owner, repo, subdir).
/// Supports: "owner/repo", "owner/repo/subdir/path", "https://github.com/owner/repo"
fn parse_github_source(source: &str) -> Result<(String, String, String), String> {
    let cleaned = source
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git");

    // Strip GitHub URL prefix if present.
    let path = if let Some(rest) = cleaned.strip_prefix("https://github.com/") {
        rest.to_string()
    } else if let Some(rest) = cleaned.strip_prefix("github.com/") {
        rest.to_string()
    } else {
        cleaned.to_string()
    };

    let parts: Vec<&str> = path.splitn(3, '/').collect();
    match parts.len() {
        0 | 1 => Err("형식: owner/repo 또는 owner/repo/subdir".to_string()),
        2 => Ok((parts[0].to_string(), parts[1].to_string(), String::new())),
        _ => Ok((
            parts[0].to_string(),
            parts[1].to_string(),
            parts[2].to_string(),
        )),
    }
}

// ── Internal ────────────────────────────────────────────────────────────

fn load_plugins_from_dir(dir: &Path) -> HashMap<String, Plugin> {
    let mut plugins = HashMap::new();

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return plugins,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let manifest_path = path.join("plugin.toml");
        if !manifest_path.exists() {
            continue;
        }

        match load_single_plugin(&path) {
            Ok(plugin) => {
                let missing = plugin.check_requirements();
                if !missing.is_empty() {
                    warn!(
                        plugin = %plugin.manifest.name,
                        missing = ?missing,
                        "plugin has unmet requirements"
                    );
                }
                info!(
                    name = %plugin.manifest.name,
                    version = %plugin.manifest.version,
                    tools = plugin.tools.len(),
                    agents = plugin.agents.len(),
                    enabled = plugin.manifest.enabled,
                    "loaded plugin"
                );
                plugins.insert(plugin.manifest.name.clone(), plugin);
            }
            Err(e) => {
                warn!(path = %path.display(), "failed to load plugin: {e}");
            }
        }
    }

    plugins
}

fn load_single_plugin(dir: &Path) -> Result<Plugin, String> {
    let manifest = parse_manifest(&dir.join("plugin.toml"))
        .ok_or("failed to parse plugin.toml")?;

    // Load tools from plugin/tools/*.toml.
    let tools_dir = dir.join("tools");
    let tools = if tools_dir.exists() {
        load_plugin_skills(&tools_dir, dir)
    } else {
        vec![]
    };

    // Load agents from plugin/agents/*.toml.
    let agents_dir = dir.join("agents");
    let agents = if agents_dir.exists() {
        load_plugin_agents(&agents_dir, dir)
    } else {
        vec![]
    };

    Ok(Plugin {
        manifest,
        path: dir.to_path_buf(),
        tools,
        agents,
    })
}

fn parse_manifest(path: &Path) -> Option<PluginManifest> {
    let content = fs::read_to_string(path).ok()?;
    toml::from_str(&content).ok()
}

/// Load skill/tool definitions from a plugin's tools/ directory.
/// Adjusts command paths to be relative to the plugin's bin/ directory.
fn load_plugin_skills(dir: &Path, plugin_dir: &Path) -> Vec<CustomSkill> {
    let mut skills = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return skills,
    };

    // Create a temporary SkillStore-compatible directory scan.
    // We reuse the existing TOML parsing by reading each file.
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }

        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Parse using the same format as ~/.sam/tools/*.toml.
        let parsed: Result<PluginToolToml, _> = toml::from_str(&content);
        match parsed {
            Ok(tool) => {
                // Resolve command path: if relative, prepend plugin bin dir.
                let command = if tool.exec.command.starts_with('/')
                    || tool.exec.command.starts_with('~')
                {
                    tool.exec.command
                } else {
                    let bin_path = plugin_dir.join("bin").join(&tool.exec.command);
                    if bin_path.exists() {
                        bin_path.to_string_lossy().to_string()
                    } else {
                        tool.exec.command // keep as-is (might be on PATH)
                    }
                };

                let schema = build_schema(&tool.input_schema);

                skills.push(CustomSkill {
                    name: tool.name,
                    description: tool.description,
                    input_schema: schema,
                    exec: crate::skill_store::SkillExec {
                        command,
                        args: tool.exec.args,
                        timeout_secs: tool.exec.timeout_secs,
                        env: tool.exec.env,
                    },
                });
            }
            Err(e) => {
                warn!(path = %path.display(), "failed to parse plugin tool: {e}");
            }
        }
    }

    skills
}

/// Load agent definitions from a plugin's agents/ directory.
fn load_plugin_agents(dir: &Path, plugin_dir: &Path) -> Vec<AgentDef> {
    let mut agents = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return agents,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }

        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        match toml::from_str::<AgentDef>(&content) {
            Ok(mut agent) => {
                // Resolve prompt_file relative to plugin's prompts/ dir.
                let prompt_path = plugin_dir.join("prompts").join(&agent.prompt_file);
                if prompt_path.exists() {
                    agent.prompt_file = prompt_path.to_string_lossy().to_string();
                }
                agents.push(agent);
            }
            Err(e) => {
                warn!(path = %path.display(), "failed to parse plugin agent: {e}");
            }
        }
    }

    agents
}

/// Simple `which` equivalent — check if a command exists on PATH.
fn which(cmd: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let full = dir.join(cmd);
            if full.exists() { Some(full) } else { None }
        })
    })
}

/// Recursively copy a directory.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

// ── Plugin TOML types (mirrors skill_store format) ──────────────────────

#[derive(Deserialize)]
struct PluginToolToml {
    name: String,
    description: String,
    input_schema: Option<PluginInputSchema>,
    exec: PluginExecToml,
}

#[derive(Deserialize)]
struct PluginInputSchema {
    #[serde(rename = "type")]
    schema_type: Option<String>,
    properties: Option<HashMap<String, serde_json::Value>>,
    required: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct PluginExecToml {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default = "default_plugin_timeout")]
    timeout_secs: u64,
    #[serde(default)]
    env: HashMap<String, String>,
}

fn default_plugin_timeout() -> u64 {
    30
}

fn build_schema(schema: &Option<PluginInputSchema>) -> serde_json::Value {
    match schema {
        Some(s) => {
            let mut obj = serde_json::json!({
                "type": s.schema_type.as_deref().unwrap_or("object"),
            });
            if let Some(ref props) = s.properties {
                obj["properties"] = serde_json::to_value(props).unwrap_or_default();
            }
            if let Some(ref req) = s.required {
                obj["required"] = serde_json::to_value(req).unwrap_or_default();
            }
            obj
        }
        None => serde_json::json!({ "type": "object", "properties": {} }),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn create_test_plugin(dir: &Path) {
        // plugin.toml
        let mut f = fs::File::create(dir.join("plugin.toml")).unwrap();
        write!(
            f,
            r#"
name = "test_plugin"
version = "0.1.0"
description = "A test plugin"
author = "test"
requires = []
enabled = true
"#
        )
        .unwrap();

        // tools/
        let tools_dir = dir.join("tools");
        fs::create_dir_all(&tools_dir).unwrap();
        let mut tf = fs::File::create(tools_dir.join("hello.toml")).unwrap();
        write!(
            tf,
            r#"
name = "plugin_hello"
description = "Says hello from plugin"

[exec]
command = "echo"
args = ["hello from plugin"]
"#
        )
        .unwrap();
    }

    #[test]
    fn parse_plugin_manifest() {
        let toml = r#"
name = "weather"
version = "1.0.0"
description = "Weather plugin"
author = "Paul"
requires = ["curl"]
enabled = true
"#;
        let manifest: PluginManifest = toml::from_str(toml).unwrap();
        assert_eq!(manifest.name, "weather");
        assert_eq!(manifest.version, "1.0.0");
        assert_eq!(manifest.requires, vec!["curl"]);
        assert!(manifest.enabled);
    }

    #[test]
    fn load_plugin_from_dir() {
        let tmp = std::env::temp_dir().join("sam_test_plugin");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        create_test_plugin(&tmp);

        let plugin = load_single_plugin(&tmp).unwrap();
        assert_eq!(plugin.manifest.name, "test_plugin");
        assert_eq!(plugin.tools.len(), 1);
        assert_eq!(plugin.tools[0].name, "plugin_hello");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn plugin_store_empty_dir() {
        let tmp = std::env::temp_dir().join("sam_test_plugins_empty");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let plugins = load_plugins_from_dir(&tmp);
        assert!(plugins.is_empty());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn plugin_tool_definitions() {
        let tmp = std::env::temp_dir().join("sam_test_plugin_defs");
        let _ = fs::remove_dir_all(&tmp);
        let plugin_dir = tmp.join("my_plugin");
        fs::create_dir_all(&plugin_dir).unwrap();

        create_test_plugin(&plugin_dir);

        let plugins = load_plugins_from_dir(&tmp);
        let store = PluginStore { plugins };

        let defs = store.tool_definitions_raw();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].0, "plugin_hello");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn install_from_path_copies_plugin() {
        let src = std::env::temp_dir().join("sam_test_plugin_src");
        let dest_root = std::env::temp_dir().join("sam_test_plugin_dest");
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dest_root);
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dest_root).unwrap();

        create_test_plugin(&src);

        // Manual copy test (since install_from_path uses plugins_dir()).
        let dest = dest_root.join("test_plugin");
        copy_dir_recursive(&src, &dest).unwrap();

        assert!(dest.join("plugin.toml").exists());
        assert!(dest.join("tools").join("hello.toml").exists());

        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dest_root);
    }

    #[test]
    fn parse_github_source_owner_repo() {
        let (owner, repo, subdir) = parse_github_source("yhc007/sam-plugin-weather").unwrap();
        assert_eq!(owner, "yhc007");
        assert_eq!(repo, "sam-plugin-weather");
        assert_eq!(subdir, "");
    }

    #[test]
    fn parse_github_source_with_subdir() {
        let (owner, repo, subdir) = parse_github_source("yhc007/sam-plugins/plugins/weather").unwrap();
        assert_eq!(owner, "yhc007");
        assert_eq!(repo, "sam-plugins");
        assert_eq!(subdir, "plugins/weather");
    }

    #[test]
    fn parse_github_source_full_url() {
        let (owner, repo, subdir) = parse_github_source("https://github.com/yhc007/my-plugin.git").unwrap();
        assert_eq!(owner, "yhc007");
        assert_eq!(repo, "my-plugin");
        assert_eq!(subdir, "");
    }

    #[test]
    fn parse_github_source_invalid() {
        assert!(parse_github_source("just-a-name").is_err());
        assert!(parse_github_source("").is_err());
    }

    #[test]
    fn registry_search_matches() {
        let registry = Registry {
            version: 1,
            plugins: vec![
                RegistryEntry {
                    name: "weather".to_string(),
                    version: "1.0.0".to_string(),
                    description: "날씨 조회 플러그인".to_string(),
                    author: "paul".to_string(),
                    repo: "yhc007/sam-plugin-weather".to_string(),
                    subdir: String::new(),
                    tags: vec!["weather".to_string(), "api".to_string()],
                    min_sam_version: String::new(),
                },
                RegistryEntry {
                    name: "translator".to_string(),
                    version: "0.5.0".to_string(),
                    description: "번역 도구".to_string(),
                    author: "paul".to_string(),
                    repo: "yhc007/sam-plugin-translator".to_string(),
                    subdir: String::new(),
                    tags: vec!["translate".to_string(), "language".to_string()],
                    min_sam_version: String::new(),
                },
            ],
        };

        let results = registry.search("날씨");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "weather");

        let results = registry.search("api");
        assert_eq!(results.len(), 1);

        let results = registry.search("language");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "translator");

        let results = registry.search("xyz_nothing");
        assert!(results.is_empty());
    }

    #[test]
    fn registry_find_by_name() {
        let registry = Registry {
            version: 1,
            plugins: vec![RegistryEntry {
                name: "weather".to_string(),
                version: "1.0.0".to_string(),
                description: "Weather".to_string(),
                author: "paul".to_string(),
                repo: "yhc007/weather".to_string(),
                subdir: String::new(),
                tags: vec![],
                min_sam_version: String::new(),
            }],
        };

        assert!(registry.find("weather").is_some());
        assert!(registry.find("nonexistent").is_none());
    }
}
