//! Custom skill/tool system — loads user-defined tools from `~/.sam/tools/*.toml`.
//!
//! Skills support a `[requires]` section for dependency gating and can be
//! installed from a GitHub-based registry (`yhc007/sam-skills`).

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::paths::tools_dir;

/// Default skill registry: a GitHub repo containing individual skill directories.
pub const DEFAULT_SKILL_REGISTRY: &str = "yhc007/sam-skills";

/// Skill type: tool (external command) or prompt (system prompt injection).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillType {
    Tool,
    Prompt,
}

impl Default for SkillType {
    fn default() -> Self { Self::Tool }
}

/// A user-defined custom skill loaded from a TOML file.
#[derive(Debug, Clone)]
pub struct CustomSkill {
    pub name: String,
    pub description: String,
    pub skill_type: SkillType,
    pub input_schema: serde_json::Value,
    pub exec: SkillExec,
    pub requires: SkillRequires,
    /// For prompt skills: the markdown content injected into system prompt.
    pub prompt_content: Option<String>,
}

/// Execution configuration for a custom skill.
#[derive(Debug, Clone, Default)]
pub struct SkillExec {
    pub command: String,
    pub args: Vec<String>,
    pub timeout_secs: u64,
    pub env: HashMap<String, String>,
}

/// Dependency requirements for a skill.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SkillRequires {
    /// Binaries that must be on PATH.
    pub bins: Vec<String>,
    /// Environment variables that must be set.
    pub env_vars: Vec<String>,
    /// Installation hint shown when requirements are missing.
    pub install_hint: Option<String>,
}

impl SkillRequires {
    /// Check if all requirements are met. Returns list of missing items.
    pub fn check(&self) -> Vec<String> {
        let mut missing = Vec::new();
        for bin in &self.bins {
            if which_bin(bin).is_none() {
                missing.push(format!("bin:{bin}"));
            }
        }
        for var in &self.env_vars {
            if std::env::var(var).is_err() {
                missing.push(format!("env:{var}"));
            }
        }
        missing
    }

    pub fn is_empty(&self) -> bool {
        self.bins.is_empty() && self.env_vars.is_empty()
    }
}

/// Simple `which` — check if a command exists on PATH.
fn which_bin(cmd: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let full = dir.join(cmd);
            if full.exists() { Some(full) } else { None }
        })
    })
}

/// Store for all loaded custom skills.
#[derive(Debug, Clone)]
pub struct SkillStore {
    skills: Vec<CustomSkill>,
}

// ── TOML deserialization types ────────────────────────────────────────────

#[derive(Deserialize)]
struct SkillToml {
    name: String,
    description: String,
    #[serde(default, rename = "type")]
    skill_type: Option<String>,
    input_schema: Option<InputSchemaToml>,
    exec: Option<ExecToml>,
    #[serde(default)]
    requires: Option<RequiresToml>,
    /// For prompt skills: path to .md file (relative to tools dir).
    #[serde(default)]
    prompt_file: Option<String>,
    /// For prompt skills: inline content.
    #[serde(default)]
    prompt_content: Option<String>,
}

#[derive(Deserialize, Default)]
struct RequiresToml {
    #[serde(default)]
    bins: Vec<String>,
    #[serde(default)]
    env_vars: Vec<String>,
    #[serde(default)]
    install_hint: Option<String>,
}

#[derive(Deserialize)]
struct InputSchemaToml {
    #[serde(rename = "type")]
    schema_type: Option<String>,
    properties: Option<HashMap<String, serde_json::Value>>,
    required: Option<RequiredField>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RequiredField {
    Direct(Vec<String>),
    Wrapped { value: Vec<String> },
}

#[derive(Deserialize)]
struct ExecToml {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
    #[serde(default)]
    env: HashMap<String, String>,
}

fn default_timeout() -> u64 {
    30
}

impl SkillStore {
    /// Scan `tools_dir()` for `.toml` files and parse them into skills.
    pub fn load() -> Self {
        let dir = tools_dir();
        let skills = load_skills_from_dir(&dir);
        info!(count = skills.len(), dir = %dir.display(), "SkillStore loaded");
        Self { skills }
    }

    /// Re-scan the tools directory, replacing all loaded skills.
    pub fn reload(&mut self) {
        let dir = tools_dir();
        self.skills = load_skills_from_dir(&dir);
        info!(count = self.skills.len(), "SkillStore reloaded");
    }

    /// Return all loaded skills.
    pub fn list(&self) -> &[CustomSkill] {
        &self.skills
    }

    /// Look up a skill by name.
    pub fn get(&self, name: &str) -> Option<&CustomSkill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// Convert tool-type skills to `ToolDefinition`-compatible JSON values.
    /// Prompt-type skills are excluded (they inject into system prompt, not tool API).
    pub fn tool_definitions_raw(&self) -> Vec<(String, String, serde_json::Value)> {
        self.skills
            .iter()
            .filter(|s| s.skill_type == SkillType::Tool)
            .map(|s| (s.name.clone(), s.description.clone(), s.input_schema.clone()))
            .collect()
    }

    /// Collect all prompt-type skill contents for system prompt injection.
    /// Returns (name, description, content) tuples.
    pub fn prompt_skills(&self) -> Vec<(&str, &str, &str)> {
        self.skills
            .iter()
            .filter(|s| s.skill_type == SkillType::Prompt)
            .filter_map(|s| {
                s.prompt_content.as_deref().map(|c| {
                    (s.name.as_str(), s.description.as_str(), c)
                })
            })
            .collect()
    }

    /// Install a skill from the GitHub registry.
    ///
    /// `source` can be:
    /// - `"skill_name"` — downloads from DEFAULT_SKILL_REGISTRY/skills/skill_name/
    /// - `"owner/repo/skill_name"` — downloads from a custom repo
    pub fn install_from_registry(&mut self, source: &str) -> Result<String, String> {
        let (repo, skill_name) = parse_skill_source(source)?;
        let dir = tools_dir();
        let _ = fs::create_dir_all(&dir);

        // Check if already installed.
        let dest_toml = dir.join(format!("{skill_name}.toml"));
        if dest_toml.exists() {
            return Err(format!("스킬 '{skill_name}'이 이미 설치되어 있음. 업데이트: sam skill update {skill_name}"));
        }

        // Clone repo to temp, extract the skill.
        let tmp = std::env::temp_dir().join(format!("sam_skill_install_{skill_name}"));
        let _ = fs::remove_dir_all(&tmp);

        let repo_url = format!("https://github.com/{repo}.git");
        let output = std::process::Command::new("git")
            .args(["clone", "--depth", "1", &repo_url, &tmp.to_string_lossy()])
            .output()
            .map_err(|e| format!("git clone 실행 실패: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = fs::remove_dir_all(&tmp);
            return Err(format!("git clone 실패: {stderr}"));
        }

        // Look for the skill in skills/<name>/ directory.
        let skill_dir = tmp.join("skills").join(&skill_name);
        if !skill_dir.exists() {
            let _ = fs::remove_dir_all(&tmp);
            return Err(format!("레지스트리에서 스킬 '{skill_name}'을 찾을 수 없음"));
        }

        // Copy .toml file.
        let src_toml = skill_dir.join(format!("{skill_name}.toml"));
        let alt_toml = skill_dir.join("skill.toml");
        let toml_src = if src_toml.exists() {
            src_toml
        } else if alt_toml.exists() {
            alt_toml
        } else {
            let _ = fs::remove_dir_all(&tmp);
            return Err("스킬 디렉토리에 .toml 파일이 없음".to_string());
        };

        fs::copy(&toml_src, &dest_toml)
            .map_err(|e| format!("toml 복사 실패: {e}"))?;

        // Copy bin/ scripts if present.
        let skill_bin_dir = skill_dir.join("bin");
        if skill_bin_dir.exists() {
            let sam_bin_dir = crate::paths::sam_home().join("bin");
            let _ = fs::create_dir_all(&sam_bin_dir);
            if let Ok(entries) = fs::read_dir(&skill_bin_dir) {
                for entry in entries.flatten() {
                    let src = entry.path();
                    let dest = sam_bin_dir.join(entry.file_name());
                    let _ = fs::copy(&src, &dest);
                    // Make executable.
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(0o755));
                    }
                }
            }
        }

        // Save source info for updates.
        let source_info = serde_json::json!({
            "source": source,
            "repo": repo,
            "skill": skill_name,
            "installed_at": chrono::Local::now().to_rfc3339(),
        });
        let meta_path = dir.join(format!(".{skill_name}.source.json"));
        let _ = fs::write(&meta_path, serde_json::to_string_pretty(&source_info).unwrap_or_default());

        let _ = fs::remove_dir_all(&tmp);

        self.reload();

        match self.get(&skill_name) {
            Some(s) => Ok(format!("스킬 '{}' 설치 완료: {}", s.name, s.description)),
            None => Ok(format!("스킬 '{skill_name}' 파일 설치됨 (요구사항 미충족으로 비활성 가능)")),
        }
    }

    /// Uninstall a skill by name.
    pub fn uninstall(&mut self, name: &str) -> Result<String, String> {
        let dir = tools_dir();
        let toml_path = dir.join(format!("{name}.toml"));
        if !toml_path.exists() {
            return Err(format!("스킬 '{name}'을 찾을 수 없음"));
        }
        fs::remove_file(&toml_path).map_err(|e| format!("삭제 실패: {e}"))?;

        // Also remove source metadata.
        let meta_path = dir.join(format!(".{name}.source.json"));
        let _ = fs::remove_file(&meta_path);

        self.reload();
        Ok(format!("스킬 '{name}' 삭제 완료"))
    }

    /// Update a skill by re-downloading from its source.
    pub fn update(&mut self, name: &str) -> Result<String, String> {
        let dir = tools_dir();
        let meta_path = dir.join(format!(".{name}.source.json"));
        if !meta_path.exists() {
            return Err(format!("스킬 '{name}'의 설치 소스 정보 없음. 수동 재설치 필요."));
        }

        let data = fs::read_to_string(&meta_path)
            .map_err(|e| format!("소스 정보 읽기 실패: {e}"))?;
        let info: serde_json::Value = serde_json::from_str(&data)
            .map_err(|_| "소스 정보 파싱 실패".to_string())?;
        let source = info["source"].as_str().unwrap_or("").to_string();
        if source.is_empty() {
            return Err("소스 정보가 비어있음".to_string());
        }

        // Remove old files.
        let toml_path = dir.join(format!("{name}.toml"));
        let _ = fs::remove_file(&toml_path);
        let _ = fs::remove_file(&meta_path);

        // Re-install.
        self.install_from_registry(&source)
    }

}

/// An entry in the skill registry index (registry.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRegistryEntry {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// The skill registry index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRegistry {
    #[serde(default = "default_version")]
    pub version: u32,
    pub skills: Vec<SkillRegistryEntry>,
}

fn default_version() -> u32 { 1 }

/// Parse skill source into (repo, skill_name).
/// - `"calculator"` → (DEFAULT_SKILL_REGISTRY, "calculator")
/// - `"owner/repo/calculator"` → ("owner/repo", "calculator")
fn parse_skill_source(source: &str) -> Result<(String, String), String> {
    let cleaned = source.trim().trim_end_matches('/');
    let parts: Vec<&str> = cleaned.split('/').collect();
    match parts.len() {
        1 => Ok((DEFAULT_SKILL_REGISTRY.to_string(), parts[0].to_string())),
        3 => Ok((
            format!("{}/{}", parts[0], parts[1]),
            parts[2].to_string(),
        )),
        _ => Err("형식: skill_name 또는 owner/repo/skill_name".to_string()),
    }
}

/// Interpolate `{{input.field_name}}` placeholders in a string using values
/// from the provided JSON object.
pub fn interpolate_args(template: &str, input: &serde_json::Value) -> String {
    let mut result = template.to_string();
    // Find all {{input.xxx}} patterns and replace them.
    loop {
        let start = result.find("{{input.");
        if start.is_none() {
            break;
        }
        let start = start.unwrap();
        let after_prefix = start + "{{input.".len();
        let end = result[after_prefix..].find("}}");
        if end.is_none() {
            break;
        }
        let end = after_prefix + end.unwrap();
        let field_name = &result[after_prefix..end];
        let value = match &input[field_name] {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        };
        result = format!("{}{}{}", &result[..start], value, &result[end + 2..]);
    }
    result
}

// ── Internal helpers ──────────────────────────────────────────────────────

fn load_skills_from_dir(dir: &PathBuf) -> Vec<CustomSkill> {
    let mut skills = Vec::new();

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => {
            // Directory doesn't exist yet — that's fine.
            return skills;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        match load_single_skill(&path) {
            Ok(skill) => {
                // Check requirements before loading.
                let missing = skill.requires.check();
                if missing.is_empty() {
                    info!(name = %skill.name, path = %path.display(), "loaded custom skill");
                    skills.push(skill);
                } else {
                    let hint = skill.requires.install_hint.as_deref().unwrap_or("");
                    warn!(
                        name = %skill.name,
                        missing = ?missing,
                        hint = hint,
                        "skill disabled: unmet requirements"
                    );
                }
            }
            Err(e) => {
                warn!(path = %path.display(), "failed to parse skill TOML: {e}");
            }
        }
    }

    skills
}

fn load_single_skill(path: &PathBuf) -> Result<CustomSkill, String> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("read error: {e}"))?;
    let toml_val: SkillToml = toml::from_str(&content)
        .map_err(|e| format!("parse error: {e}"))?;

    let requires = match toml_val.requires {
        Some(r) => SkillRequires {
            bins: r.bins,
            env_vars: r.env_vars,
            install_hint: r.install_hint,
        },
        None => SkillRequires::default(),
    };

    // Determine skill type.
    let skill_type = match toml_val.skill_type.as_deref() {
        Some("prompt") => SkillType::Prompt,
        _ => SkillType::Tool,
    };

    if skill_type == SkillType::Prompt {
        // Prompt skill: load markdown content from file or inline.
        let prompt_content = if let Some(ref pf) = toml_val.prompt_file {
            // Resolve relative to the .toml file's parent dir, or tools_dir.
            let base = path.parent().unwrap_or_else(|| std::path::Path::new("."));
            let prompt_path = base.join(pf);
            if prompt_path.exists() {
                Some(fs::read_to_string(&prompt_path)
                    .map_err(|e| format!("prompt file read error: {e}"))?)
            } else {
                // Try ~/.sam/prompts/
                let alt = crate::paths::prompts_dir().join(pf);
                if alt.exists() {
                    Some(fs::read_to_string(&alt)
                        .map_err(|e| format!("prompt file read error: {e}"))?)
                } else {
                    return Err(format!("prompt file not found: {}", prompt_path.display()));
                }
            }
        } else {
            toml_val.prompt_content.clone()
        };

        if prompt_content.is_none() {
            return Err("prompt skill requires 'prompt_file' or 'prompt_content'".to_string());
        }

        return Ok(CustomSkill {
            name: toml_val.name,
            description: toml_val.description,
            skill_type,
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
            exec: SkillExec::default(),
            requires,
            prompt_content,
        });
    }

    // Tool skill: requires exec section.
    let exec_toml = toml_val.exec
        .ok_or("tool skill requires [exec] section")?;

    let input_schema = build_input_schema(toml_val.input_schema);

    Ok(CustomSkill {
        name: toml_val.name,
        description: toml_val.description,
        skill_type,
        input_schema,
        exec: SkillExec {
            command: exec_toml.command,
            args: exec_toml.args,
            timeout_secs: exec_toml.timeout_secs,
            env: exec_toml.env,
        },
        requires,
        prompt_content: None,
    })
}

fn build_input_schema(schema: Option<InputSchemaToml>) -> serde_json::Value {
    let Some(schema) = schema else {
        return serde_json::json!({
            "type": "object",
            "properties": {}
        });
    };

    let schema_type = schema.schema_type.unwrap_or_else(|| "object".to_string());
    let properties = schema.properties.unwrap_or_default();
    let required: Vec<String> = match schema.required {
        Some(RequiredField::Direct(v)) => v,
        Some(RequiredField::Wrapped { value }) => value,
        None => Vec::new(),
    };

    let mut obj = serde_json::json!({
        "type": schema_type,
        "properties": properties,
    });

    if !required.is_empty() {
        obj["required"] = serde_json::json!(required);
    }

    obj
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolate_replaces_fields() {
        let input = serde_json::json!({"city": "Seoul", "count": 5});
        let result = interpolate_args("wttr.in/{{input.city}}?n={{input.count}}", &input);
        assert_eq!(result, "wttr.in/Seoul?n=5");
    }

    #[test]
    fn interpolate_missing_field() {
        let input = serde_json::json!({"city": "Seoul"});
        let result = interpolate_args("{{input.missing}}", &input);
        assert_eq!(result, "");
    }

    #[test]
    fn build_schema_empty() {
        let schema = build_input_schema(None);
        assert_eq!(schema["type"], "object");
    }

    #[test]
    fn load_from_nonexistent_dir() {
        let store = SkillStore {
            skills: load_skills_from_dir(&PathBuf::from("/nonexistent/path")),
        };
        assert!(store.list().is_empty());
    }

    /// Integration test: loads ALL skills from the real ~/.sam/tools directory.
    /// Ensures every TOML file in the directory parses without error.
    #[test]
    fn load_real_skills_directory() {
        let dir = crate::paths::tools_dir();
        if !dir.exists() {
            // Skip if tools dir doesn't exist (CI).
            return;
        }
        let store = SkillStore::load();
        let skills = store.list();
        // We should have at least the media + utility skills.
        assert!(
            skills.len() >= 5,
            "expected at least 5 skills, found {}",
            skills.len()
        );
        // Verify each skill has non-empty name and description.
        for skill in skills {
            assert!(!skill.name.is_empty(), "skill has empty name");
            assert!(!skill.description.is_empty(), "skill {} has empty description", skill.name);
            assert!(!skill.exec.command.is_empty(), "skill {} has empty command", skill.name);
        }
    }

    /// Test hot-reload: load, then reload and verify same count.
    #[test]
    fn reload_preserves_skills() {
        let dir = crate::paths::tools_dir();
        if !dir.exists() {
            return;
        }
        let mut store = SkillStore::load();
        let count1 = store.list().len();
        store.reload();
        let count2 = store.list().len();
        assert_eq!(count1, count2, "reload changed skill count");
    }

    #[test]
    fn parse_skill_toml() {
        let toml_str = r#"
name = "test_skill"
description = "A test skill"

[input_schema]
type = "object"
[input_schema.properties.city]
type = "string"
description = "City name"
[input_schema.required]
value = ["city"]

[exec]
command = "echo"
args = ["{{input.city}}"]
timeout_secs = 10
"#;
        let toml_val: SkillToml = toml::from_str(toml_str).unwrap();
        assert_eq!(toml_val.name, "test_skill");
        let exec = toml_val.exec.unwrap();
        assert_eq!(exec.command, "echo");
        assert_eq!(exec.timeout_secs, 10);

        let schema = build_input_schema(toml_val.input_schema);
        assert_eq!(schema["required"], serde_json::json!(["city"]));
    }
}
