//! Built-in tools for Sam's Claude tool_use integration.
//!
//! Tools: memory_recall, memory_store, current_time, run_command,
//! read_file, write_file, claude_code.

use std::path::Path;
use std::time::Duration;

use serde_json::json;
use tokio::process::Command;
use tracing::{info, warn};

use sam_core::SamConfig;
use sam_memory_adapter::MemoryAdapter;

use crate::types::ToolDefinition;

/// Maximum number of tool-use loop iterations per user message.
pub const MAX_TOOL_ROUNDS: usize = 10;

/// Maximum output bytes returned from a command or file read.
const MAX_OUTPUT_BYTES: usize = 8_000;

/// Runtime context passed to tool execution.
pub struct ToolContext<'a> {
    pub memory: Option<&'a mut MemoryAdapter>,
    pub config: &'a SamConfig,
}

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
        ToolDefinition {
            name: "run_command".to_string(),
            description: "쉘 명령을 실행한다. 파일 목록 확인, git 작업, 빌드, 테스트 등에 사용.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "실행할 쉘 명령"
                    },
                    "working_dir": {
                        "type": "string",
                        "description": "작업 디렉토리 (기본: 홈)"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "타임아웃 초 (기본 30, 최대 300)",
                        "default": 30
                    }
                },
                "required": ["command"]
            }),
        },
        ToolDefinition {
            name: "read_file".to_string(),
            description: "파일 내용을 읽는다.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "읽을 파일 경로"
                    },
                    "max_lines": {
                        "type": "integer",
                        "description": "최대 줄 수 (기본 200)",
                        "default": 200
                    }
                },
                "required": ["path"]
            }),
        },
        ToolDefinition {
            name: "write_file".to_string(),
            description: "파일에 내용을 쓴다. 새 파일 생성이나 기존 파일 덮어쓰기.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "쓸 파일 경로"
                    },
                    "content": {
                        "type": "string",
                        "description": "파일 내용"
                    }
                },
                "required": ["path", "content"]
            }),
        },
        ToolDefinition {
            name: "claude_code".to_string(),
            description: "Claude Code를 실행해서 복잡한 코딩 작업을 수행한다. 새 프로젝트 생성, 코드 리팩토링, 버그 수정 등.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Claude Code에 전달할 작업 설명"
                    },
                    "working_dir": {
                        "type": "string",
                        "description": "작업 디렉토리 (기본: ~/work)"
                    }
                },
                "required": ["prompt"]
            }),
        },
    ]
}

/// Execute a built-in tool by name. Returns the tool result as a string.
pub async fn execute_builtin(
    name: &str,
    input: &serde_json::Value,
    ctx: &mut ToolContext<'_>,
) -> Result<String, String> {
    info!(tool = name, "executing built-in tool");

    match name {
        "memory_recall" => exec_memory_recall(input, ctx),
        "memory_store" => exec_memory_store(input, ctx),
        "current_time" => exec_current_time(),
        "run_command" => exec_run_command(input, ctx).await,
        "read_file" => exec_read_file(input),
        "write_file" => exec_write_file(input),
        "claude_code" => exec_claude_code(input, ctx).await,
        _ => Err(format!("unknown tool: {name}")),
    }
}

// ── Memory tools ───────────────────────────────────────────────────────

fn exec_memory_recall(input: &serde_json::Value, ctx: &mut ToolContext<'_>) -> Result<String, String> {
    let mem = ctx.memory.as_deref_mut().ok_or("memory system unavailable")?;
    let query = input["query"].as_str().ok_or("missing 'query' parameter")?;
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

fn exec_memory_store(input: &serde_json::Value, ctx: &mut ToolContext<'_>) -> Result<String, String> {
    let mem = ctx.memory.as_deref_mut().ok_or("memory system unavailable")?;
    let text = input["text"].as_str().ok_or("missing 'text' parameter")?;
    let tags: Vec<String> = input["tags"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    match mem.store(text, tags) {
        Ok(id) => Ok(format!("저장 완료 (id: {id})")),
        Err(e) => Err(format!("저장 실패: {e}")),
    }
}

fn exec_current_time() -> Result<String, String> {
    let now = chrono::Local::now();
    Ok(now.format("%Y-%m-%d %H:%M:%S (%A)").to_string())
}

// ── Command execution ──────────────────────────────────────────────────

/// Check if a command matches any destructive pattern from config.
fn is_destructive(command: &str, patterns: &[String]) -> Option<String> {
    let cmd_lower = command.to_lowercase();
    for pat in patterns {
        if cmd_lower.contains(&pat.to_lowercase()) {
            return Some(pat.clone());
        }
    }
    None
}

async fn exec_run_command(input: &serde_json::Value, ctx: &ToolContext<'_>) -> Result<String, String> {
    let command = input["command"].as_str().ok_or("missing 'command' parameter")?;

    // Safety check against destructive patterns.
    if let Some(pattern) = is_destructive(command, &ctx.config.safety.destructive_patterns) {
        return Err(format!(
            "차단됨: 명령에 위험한 패턴 '{pattern}'이 포함되어 있습니다. 이 명령은 실행할 수 없습니다."
        ));
    }

    let working_dir = input["working_dir"]
        .as_str()
        .map(|s| sam_core::expand_tilde(s))
        .unwrap_or_else(|| "/Volumes/T7/Sam".to_string());

    let timeout = input["timeout_secs"]
        .as_u64()
        .unwrap_or(30)
        .min(300);

    info!(command = command, cwd = %working_dir, "run_command");

    let result = tokio::time::timeout(
        Duration::from_secs(timeout),
        Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&working_dir)
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let code = output.status.code().unwrap_or(-1);

            let mut result = format!("[exit code: {code}]\n");
            if !stdout.is_empty() {
                result.push_str(&truncate_output(&stdout, MAX_OUTPUT_BYTES));
            }
            if !stderr.is_empty() {
                result.push_str("\n[stderr]\n");
                result.push_str(&truncate_output(&stderr, MAX_OUTPUT_BYTES / 2));
            }
            Ok(result)
        }
        Ok(Err(e)) => Err(format!("명령 실행 실패: {e}")),
        Err(_) => Err(format!("타임아웃: {timeout}초 초과")),
    }
}

// ── File operations ────────────────────────────────────────────────────

fn exec_read_file(input: &serde_json::Value) -> Result<String, String> {
    let path_str = input["path"].as_str().ok_or("missing 'path' parameter")?;
    let path = sam_core::expand_tilde(path_str);
    let max_lines = input["max_lines"].as_u64().unwrap_or(200) as usize;

    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("파일 읽기 실패 '{path}': {e}"))?;

    let lines: Vec<&str> = content.lines().collect();
    if lines.len() > max_lines {
        let truncated: String = lines[..max_lines].join("\n");
        Ok(format!(
            "{truncated}\n\n... ({} 줄 중 {max_lines}줄만 표시)",
            lines.len()
        ))
    } else {
        Ok(content)
    }
}

fn exec_write_file(input: &serde_json::Value) -> Result<String, String> {
    let path_str = input["path"].as_str().ok_or("missing 'path' parameter")?;
    let content = input["content"].as_str().ok_or("missing 'content' parameter")?;
    let path = sam_core::expand_tilde(path_str);

    // Create parent directories.
    if let Some(parent) = Path::new(&path).parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("디렉토리 생성 실패: {e}"))?;
    }

    std::fs::write(&path, content)
        .map_err(|e| format!("파일 쓰기 실패 '{path}': {e}"))?;

    let lines = content.lines().count();
    let bytes = content.len();
    Ok(format!("파일 저장 완료: {path} ({lines}줄, {bytes}바이트)"))
}

// ── Claude Code ────────────────────────────────────────────────────────

async fn exec_claude_code(input: &serde_json::Value, ctx: &ToolContext<'_>) -> Result<String, String> {
    let prompt = input["prompt"].as_str().ok_or("missing 'prompt' parameter")?;
    let cc = &ctx.config.claude_code;

    let working_dir = input["working_dir"]
        .as_str()
        .map(|s| sam_core::expand_tilde(s))
        .unwrap_or_else(|| "/Volumes/T7/Sam".to_string());

    // Ensure working directory exists.
    std::fs::create_dir_all(&working_dir)
        .map_err(|e| format!("작업 디렉토리 생성 실패: {e}"))?;

    let binary = cc.resolved_binary();
    if !binary.exists() {
        return Err(format!(
            "Claude Code 바이너리를 찾을 수 없습니다: {}",
            binary.display()
        ));
    }

    let timeout_secs = cc.hard_timeout_secs;

    info!(
        prompt_len = prompt.len(),
        cwd = %working_dir,
        binary = %binary.display(),
        "launching Claude Code"
    );

    let result = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        Command::new(&binary)
            .arg("--print")
            .arg("--output-format")
            .arg("text")
            .arg("--max-turns")
            .arg("20")
            .arg(prompt)
            .current_dir(&working_dir)
            .env("CLAUDE_CODE_ENTRYPOINT", "sam-agent")
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let code = output.status.code().unwrap_or(-1);

            if code != 0 {
                warn!(code = code, "Claude Code exited with error");
            }

            let mut result = String::new();
            if !stdout.is_empty() {
                result.push_str(&truncate_output(&stdout, MAX_OUTPUT_BYTES));
            }
            if !stderr.is_empty() && code != 0 {
                result.push_str("\n[stderr]\n");
                result.push_str(&truncate_output(&stderr, MAX_OUTPUT_BYTES / 4));
            }
            if result.is_empty() {
                result = "Claude Code가 완료되었지만 출력이 없습니다.".to_string();
            }
            Ok(result)
        }
        Ok(Err(e)) => Err(format!("Claude Code 실행 실패: {e}")),
        Err(_) => Err(format!("Claude Code 타임아웃: {timeout_secs}초 초과")),
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn truncate_output(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        s.to_string()
    } else {
        // Find a safe char boundary.
        let mut end = max_bytes;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...\n[출력이 {max_bytes}바이트로 잘림]", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_definitions_are_valid() {
        let defs = builtin_tool_definitions();
        assert_eq!(defs.len(), 7);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"memory_recall"));
        assert!(names.contains(&"run_command"));
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"claude_code"));
    }

    #[test]
    fn current_time_works() {
        let result = exec_current_time();
        assert!(result.is_ok());
        let time_str = result.unwrap();
        assert!(time_str.contains('-'), "expected date in output: {time_str}");
        assert!(time_str.contains('(') && time_str.contains(')'), "day name: {time_str}");
    }

    #[test]
    fn unknown_tool_is_rejected() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let config = SamConfig::default();
        let mut ctx = ToolContext { memory: None, config: &config };
        let input = json!({});
        let result = rt.block_on(execute_builtin("nonexistent", &input, &mut ctx));
        assert!(result.is_err());
    }

    #[test]
    fn memory_recall_without_adapter_returns_error() {
        let config = SamConfig::default();
        let mut ctx = ToolContext { memory: None, config: &config };
        let input = json!({"query": "test"});
        let result = exec_memory_recall(&input, &mut ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unavailable"));
    }

    #[test]
    fn memory_store_without_adapter_returns_error() {
        let config = SamConfig::default();
        let mut ctx = ToolContext { memory: None, config: &config };
        let input = json!({"text": "remember this"});
        let result = exec_memory_store(&input, &mut ctx);
        assert!(result.is_err());
    }

    #[test]
    fn destructive_command_is_blocked() {
        let patterns = vec!["rm -rf".to_string(), "sudo".to_string()];
        assert!(is_destructive("rm -rf /", &patterns).is_some());
        assert!(is_destructive("sudo reboot", &patterns).is_some());
        assert!(is_destructive("ls -la", &patterns).is_none());
        assert!(is_destructive("cargo build", &patterns).is_none());
    }

    #[test]
    fn read_nonexistent_file_returns_error() {
        let input = json!({"path": "/nonexistent/file.txt"});
        let result = exec_read_file(&input);
        assert!(result.is_err());
    }

    #[test]
    fn write_and_read_file_roundtrip() {
        let tmp = std::env::temp_dir().join("sam-test-write-read.txt");
        let path = tmp.to_string_lossy().to_string();
        let content = "hello\nworld\n";

        let write_input = json!({"path": path, "content": content});
        let result = exec_write_file(&write_input);
        assert!(result.is_ok(), "write failed: {:?}", result);

        let read_input = json!({"path": path});
        let result = exec_read_file(&read_input).unwrap();
        assert_eq!(result, content);

        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn run_command_works() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let config = SamConfig::default();
        let ctx = ToolContext { memory: None, config: &config };
        let input = json!({"command": "echo hello"});
        let result = rt.block_on(exec_run_command(&input, &ctx));
        assert!(result.is_ok());
        assert!(result.unwrap().contains("hello"));
    }

    #[test]
    fn run_command_blocked_by_safety() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut config = SamConfig::default();
        config.safety.destructive_patterns = vec!["rm -rf".to_string()];
        let ctx = ToolContext { memory: None, config: &config };
        let input = json!({"command": "rm -rf /"});
        let result = rt.block_on(exec_run_command(&input, &ctx));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("차단"));
    }

    #[test]
    fn truncate_output_preserves_short_text() {
        let s = "short text";
        assert_eq!(truncate_output(s, 100), s);
    }

    #[test]
    fn truncate_output_cuts_long_text() {
        let s = "a".repeat(200);
        let result = truncate_output(&s, 50);
        assert!(result.len() < 200);
        assert!(result.contains("잘림"));
    }
}
