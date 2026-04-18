//! Built-in tools for Sam's Claude tool_use integration.

use serde_json::json;
use tracing::info;

use sam_memory_adapter::MemoryAdapter;

use crate::types::ToolDefinition;

/// Maximum number of tool-use loop iterations per user message.
pub const MAX_TOOL_ROUNDS: usize = 5;

/// Return the list of built-in tool definitions for the Claude API.
pub fn builtin_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "memory_recall".to_string(),
            description: "장기 기억에서 관련 내용을 검색한다. 이전 대화나 저장된 정보를 찾을 때 사용.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "검색할 내용"
                    },
                    "k": {
                        "type": "integer",
                        "description": "반환할 최대 결과 수 (기본 5)",
                        "default": 5
                    }
                },
                "required": ["query"]
            }),
        },
        ToolDefinition {
            name: "memory_store".to_string(),
            description: "중요한 정보를 장기 기억에 저장한다. 사용자가 '기억해' 또는 중요한 사실을 알려줄 때 사용.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "저장할 내용"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "분류 태그 (선택)"
                    }
                },
                "required": ["text"]
            }),
        },
        ToolDefinition {
            name: "current_time".to_string(),
            description: "현재 날짜와 시간을 반환한다.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
    ]
}

/// Execute a built-in tool by name. Returns the tool result as a string.
pub fn execute_builtin(
    name: &str,
    input: &serde_json::Value,
    memory: Option<&mut MemoryAdapter>,
) -> Result<String, String> {
    info!(tool = name, "executing built-in tool");

    match name {
        "memory_recall" => {
            let mem = memory.ok_or("memory system unavailable")?;
            let query = input["query"]
                .as_str()
                .ok_or("missing 'query' parameter")?;
            let k = input["k"].as_u64().unwrap_or(5) as usize;
            let hits = mem.recall(query, k);
            if hits.is_empty() {
                Ok("관련 기억을 찾지 못했습니다.".to_string())
            } else {
                let mut result = String::new();
                for (i, hit) in hits.iter().enumerate() {
                    result.push_str(&format!(
                        "{}. [유사도 {:.2}] {}\n",
                        i + 1,
                        hit.similarity,
                        hit.text.replace('\n', " | "),
                    ));
                }
                Ok(result)
            }
        }
        "memory_store" => {
            let mem = memory.ok_or("memory system unavailable")?;
            let text = input["text"]
                .as_str()
                .ok_or("missing 'text' parameter")?;
            let tags: Vec<String> = input["tags"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            match mem.store(text, tags) {
                Ok(id) => Ok(format!("저장 완료 (id: {id})")),
                Err(e) => Err(format!("저장 실패: {e}")),
            }
        }
        "current_time" => {
            let now = chrono::Local::now();
            Ok(now.format("%Y-%m-%d %H:%M:%S (%A)").to_string())
        }
        _ => Err(format!("unknown tool: {name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_definitions_are_valid() {
        let defs = builtin_tool_definitions();
        assert_eq!(defs.len(), 3);
        assert_eq!(defs[0].name, "memory_recall");
        assert_eq!(defs[1].name, "memory_store");
        assert_eq!(defs[2].name, "current_time");
    }

    #[test]
    fn current_time_works_without_memory() {
        let input = json!({});
        let result = execute_builtin("current_time", &input, None);
        assert!(result.is_ok());
        let time_str = result.unwrap();
        // Should contain a date-like pattern.
        assert!(time_str.contains('-'), "expected date in output: {time_str}");
    }

    #[test]
    fn unknown_tool_returns_error() {
        let input = json!({});
        let result = execute_builtin("nonexistent", &input, None);
        assert!(result.is_err());
    }

    #[test]
    fn memory_recall_without_adapter_returns_error() {
        let input = json!({"query": "test"});
        let result = execute_builtin("memory_recall", &input, None);
        assert!(result.is_err());
    }
}
