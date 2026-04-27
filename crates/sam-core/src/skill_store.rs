//! Custom skill/tool system — loads user-defined tools from `~/.sam/tools/*.toml`.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;
use tracing::{info, warn};

use crate::paths::tools_dir;

/// A user-defined custom skill loaded from a TOML file.
#[derive(Debug, Clone)]
pub struct CustomSkill {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub exec: SkillExec,
}

/// Execution configuration for a custom skill.
#[derive(Debug, Clone)]
pub struct SkillExec {
    pub command: String,
    pub args: Vec<String>,
    pub timeout_secs: u64,
    pub env: HashMap<String, String>,
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
    input_schema: Option<InputSchemaToml>,
    exec: ExecToml,
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

    /// Convert all skills to `ToolDefinition`-compatible JSON values.
    /// Returns a vec of (name, description, input_schema) tuples that can be
    /// converted into the crate's ToolDefinition type.
    pub fn tool_definitions_raw(&self) -> Vec<(String, String, serde_json::Value)> {
        self.skills
            .iter()
            .map(|s| (s.name.clone(), s.description.clone(), s.input_schema.clone()))
            .collect()
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
                info!(name = %skill.name, path = %path.display(), "loaded custom skill");
                skills.push(skill);
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

    // Build JSON Schema from the TOML representation.
    let input_schema = build_input_schema(toml_val.input_schema);

    Ok(CustomSkill {
        name: toml_val.name,
        description: toml_val.description,
        input_schema,
        exec: SkillExec {
            command: toml_val.exec.command,
            args: toml_val.exec.args,
            timeout_secs: toml_val.exec.timeout_secs,
            env: toml_val.exec.env,
        },
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
        assert_eq!(toml_val.exec.command, "echo");
        assert_eq!(toml_val.exec.timeout_secs, 10);

        let schema = build_input_schema(toml_val.input_schema);
        assert_eq!(schema["required"], serde_json::json!(["city"]));
    }
}
