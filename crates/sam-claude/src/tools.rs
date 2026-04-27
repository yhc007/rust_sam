//! Built-in tools for Sam's Claude tool_use integration.
//!
//! Tools: memory_recall, memory_store, current_time, run_command,
//! read_file, write_file, claude_code, twitter_search,
//! schedule_reminder, list_reminders, cancel_reminder.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::TimeZone;
use serde_json::json;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{info, warn};

use sam_core::{CronSchedule, CronStore, FlowStore, SamConfig, SkillStore, new_job, parse_datetime_to_unix, interpolate_args};
use sam_memory_adapter::MemoryAdapter;

use crate::flow_runner::run_flow;
use crate::llm_client::LlmClient;
use crate::mcp::McpClient;

use crate::types::ToolDefinition;

/// Maximum number of tool-use loop iterations per user message.
pub const MAX_TOOL_ROUNDS: usize = 10;

/// Maximum output bytes returned from a command or file read.
const MAX_OUTPUT_BYTES: usize = 8_000;

/// Runtime context passed to tool execution.
pub struct ToolContext<'a> {
    pub memory: Option<&'a mut MemoryAdapter>,
    pub config: &'a SamConfig,
    pub cron_store: Option<Arc<Mutex<CronStore>>>,
    /// The iMessage handle of the user sending the message (for scheduling).
    pub sender_handle: String,
    /// Flow store for listing/running flows.
    pub flow_store: Option<Arc<Mutex<FlowStore>>>,
    /// LLM client reference for flow execution.
    pub llm_client: Option<&'a dyn LlmClient>,
    /// MCP server clients for external tool dispatch.
    pub mcp_clients: Option<Arc<Mutex<Vec<McpClient>>>>,
    /// Custom skill store for user-defined tools.
    pub skill_store: Option<Arc<Mutex<SkillStore>>>,
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
            name: "ingest_document".to_string(),
            description: "문서를 청크로 나눠 장기 기억에 저장한다 (RAG). 파일 경로나 텍스트를 입력하면 검색 가능한 형태로 저장.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "문서 출처 (파일명, URL 등)"
                    },
                    "text": {
                        "type": "string",
                        "description": "저장할 문서 텍스트 (직접 입력 시)"
                    },
                    "file_path": {
                        "type": "string",
                        "description": "읽을 파일 경로 (.txt, .md, .pdf). text 대신 사용."
                    },
                    "chunk_size": {
                        "type": "integer",
                        "description": "청크 크기 (문자 수, 기본 500)",
                        "default": 500
                    },
                    "chunk_overlap": {
                        "type": "integer",
                        "description": "청크 간 겹침 (문자 수, 기본 50)",
                        "default": 50
                    }
                },
                "required": ["source"]
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
        ToolDefinition {
            name: "twitter_search".to_string(),
            description: "트위터/X에서 최근 트윗을 검색한다. 특정 주제, 키워드, 해시태그에 대한 트윗을 찾을 때 사용.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "검색 쿼리 (예: 'AI', '#rust', 'from:elonmusk')"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "반환할 최대 트윗 수 (10-100, 기본 10)",
                        "default": 10
                    }
                },
                "required": ["query"]
            }),
        },
        ToolDefinition {
            name: "web_search".to_string(),
            description: "인터넷에서 정보를 검색한다. 최신 뉴스, 사실 확인, 모르는 주제 조사 등에 사용.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "검색 쿼리"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "최대 결과 수 (기본 5, 최대 10)",
                        "default": 5
                    }
                },
                "required": ["query"]
            }),
        },
        ToolDefinition {
            name: "schedule_reminder".to_string(),
            description: "리마인더나 반복 작업을 예약한다. 특정 시간에 알림을 보내거나 정기적으로 알림을 반복할 때 사용.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "리마인더 내용"
                    },
                    "cron_expr": {
                        "type": "string",
                        "description": "5자리 cron 표현식 (분 시 일 월 요일). 예: '0 9 * * *'은 매일 9시"
                    },
                    "datetime": {
                        "type": "string",
                        "description": "일회성 알림 시간 (ISO 8601). 예: '2026-05-01T09:00:00' 또는 '2026-05-01 09:00'"
                    },
                    "repeat": {
                        "type": "boolean",
                        "description": "반복 여부 (cron_expr 사용 시 기본 true, datetime 사용 시 기본 false)"
                    }
                },
                "required": ["message"]
            }),
        },
        ToolDefinition {
            name: "list_reminders".to_string(),
            description: "현재 예약된 리마인더 목록을 보여준다.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "cancel_reminder".to_string(),
            description: "예약된 리마인더를 취소한다. ID 또는 내용의 일부로 검색해서 삭제.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "삭제할 리마인더 ID"
                    },
                    "query": {
                        "type": "string",
                        "description": "리마인더 내용 검색어 (일부만 일치해도 삭제)"
                    }
                }
            }),
        },
        ToolDefinition {
            name: "run_flow".to_string(),
            description: "등록된 플로우(워크플로우)를 실행한다. 여러 단계를 순서대로 수행하는 자동화 파이프라인.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "실행할 플로우 이름"
                    }
                },
                "required": ["name"]
            }),
        },
        ToolDefinition {
            name: "list_flows".to_string(),
            description: "등록된 플로우(워크플로우) 목록을 보여준다.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "handoff_to_agent".to_string(),
            description: "현재 대화를 다른 전문 에이전트에게 넘긴다. 코딩은 coder, 검색은 researcher, 일정은 scheduler에게.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "description": "넘길 에이전트 이름 (coder, researcher, scheduler, default)"
                    },
                    "context": {
                        "type": "string",
                        "description": "인수인계 맥락 (다음 에이전트가 알아야 할 정보)"
                    }
                },
                "required": ["agent"]
            }),
        },
        ToolDefinition {
            name: "transfer_memo".to_string(),
            description: "다른 에이전트에게 인수인계 메모를 남긴다. handoff 전에 호출하면 받는 에이전트가 맥락을 이어받는다.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "target_agent": {
                        "type": "string",
                        "description": "메모를 받을 에이전트 이름"
                    },
                    "summary": {
                        "type": "string",
                        "description": "지금까지의 대화 맥락과 남은 작업 요약"
                    },
                    "key_facts": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "핵심 사실/결정사항 목록"
                    }
                },
                "required": ["target_agent", "summary"]
            }),
        },
        ToolDefinition {
            name: "browser".to_string(),
            description: "웹 브라우저를 제어한다. 페이지 방문, 내용 추출, 스크린샷 가능.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "description": "수행할 액션: navigate (URL 방문 + 내용 반환), screenshot (페이지 캡처), extract (CSS 선택자로 특정 요소 추출)"
                    },
                    "url": {
                        "type": "string",
                        "description": "방문할 URL"
                    },
                    "selector": {
                        "type": "string",
                        "description": "추출할 CSS 선택자 (extract 시)"
                    }
                },
                "required": ["action", "url"]
            }),
        },
        ToolDefinition {
            name: "notion_create_page".to_string(),
            description: "Notion에 새 페이지를 생성한다. 메모, 문서, 아이디어 정리 등에 사용.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "페이지 제목"
                    },
                    "content": {
                        "type": "string",
                        "description": "페이지 내용 (텍스트)"
                    }
                },
                "required": ["title", "content"]
            }),
        },
        ToolDefinition {
            name: "generate_image".to_string(),
            description: "AI 이미지를 생성한다. DALL-E API를 사용해 프롬프트 기반 이미지 생성. 결과는 iMessage로 첨부 전송된다.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "생성할 이미지를 설명하는 프롬프트 (영어 권장)"
                    },
                    "size": {
                        "type": "string",
                        "description": "이미지 크기: 1024x1024, 1792x1024, 1024x1792 (기본: 1024x1024)",
                        "default": "1024x1024"
                    },
                    "style": {
                        "type": "string",
                        "description": "스타일: natural 또는 vivid (기본: vivid)",
                        "default": "vivid"
                    }
                },
                "required": ["prompt"]
            }),
        },
        ToolDefinition {
            name: "generate_chart".to_string(),
            description: "데이터 차트/그래프를 생성한다. matplotlib로 PNG 이미지를 만들어 iMessage로 첨부 전송. 막대, 선, 파이 차트 등 지원.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "chart_type": {
                        "type": "string",
                        "description": "차트 종류: bar, line, pie, scatter, histogram"
                    },
                    "title": {
                        "type": "string",
                        "description": "차트 제목"
                    },
                    "data": {
                        "type": "object",
                        "description": "차트 데이터. labels(배열)와 values(배열 또는 {시리즈명: 배열} 객체) 포함",
                        "properties": {
                            "labels": {
                                "type": "array",
                                "items": { "type": "string" }
                            },
                            "values": {
                                "description": "숫자 배열 또는 {시리즈명: 숫자배열} 객���"
                            }
                        }
                    },
                    "python_code": {
                        "type": "string",
                        "description": "직접 matplotlib Python 코드를 작성 (data/chart_type 대신 사용 가능). plt.savefig(output_path)로 끝나야 함."
                    }
                },
                "required": ["title"]
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
        "run_flow" => exec_run_flow(input, ctx).await,
        "list_flows" => exec_list_flows(ctx).await,
        _ => execute_builtin_no_flow(name, input, ctx).await,
    }
}

/// Execute a built-in tool excluding flow tools. Used by flow_runner to
/// avoid async recursion (run_flow → execute_builtin → run_flow).
pub async fn execute_builtin_no_flow(
    name: &str,
    input: &serde_json::Value,
    ctx: &mut ToolContext<'_>,
) -> Result<String, String> {
    match name {
        "memory_recall" => exec_memory_recall(input, ctx),
        "memory_store" => exec_memory_store(input, ctx),
        "ingest_document" => exec_ingest_document(input, ctx).await,
        "current_time" => exec_current_time(),
        "run_command" => exec_run_command(input, ctx).await,
        "read_file" => exec_read_file(input),
        "write_file" => exec_write_file(input),
        "claude_code" => exec_claude_code(input, ctx).await,
        "twitter_search" => exec_twitter_search(input, ctx).await,
        "web_search" => exec_web_search(input, ctx).await,
        "schedule_reminder" => exec_schedule_reminder(input, ctx).await,
        "list_reminders" => exec_list_reminders(ctx).await,
        "cancel_reminder" => exec_cancel_reminder(input, ctx).await,
        "notion_create_page" => exec_notion_create_page(input, ctx).await,
        "handoff_to_agent" => exec_handoff(input),
        "transfer_memo" => exec_transfer_memo(input, ctx),
        "browser" => exec_browser(input, ctx).await,
        "generate_image" => exec_generate_image(input, ctx).await,
        "generate_chart" => exec_generate_chart(input, ctx).await,
        _ => {
            // Try custom skill store first.
            if let Some(skill_store) = &ctx.skill_store {
                let store = skill_store.lock().await;
                if let Some(skill) = store.get(name).cloned() {
                    drop(store); // release lock before executing
                    info!(tool = name, command = %skill.exec.command, "dispatching to custom skill");
                    return execute_custom_skill(&skill, input).await;
                }
            }

            // Try MCP servers for unknown tool names.
            if let Some(mcp_clients) = &ctx.mcp_clients {
                let mut clients = mcp_clients.lock().await;
                for client in clients.iter_mut() {
                    if client.has_tool(name) {
                        info!(tool = name, mcp_server = %client.name, "dispatching to MCP server");
                        return client.call_tool(name, input.clone()).await
                            .map_err(|e| format!("MCP tool error: {e}"));
                    }
                }
            }
            Err(format!("unknown tool: {name}"))
        }
    }
}

// ── Memory tools ───────────────────────────────────────────────────────

fn exec_memory_recall(input: &serde_json::Value, ctx: &mut ToolContext<'_>) -> Result<String, String> {
    let mem = ctx.memory.as_deref_mut().ok_or("memory system unavailable")?;
    let query = input["query"].as_str().ok_or("missing 'query' parameter")?;
    let k = input["k"].as_u64().unwrap_or(5) as usize;
    // Use hybrid recall (vector + keyword) for better results.
    let hits = mem.recall_hybrid(query, k);
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

async fn exec_ingest_document(
    input: &serde_json::Value,
    ctx: &mut ToolContext<'_>,
) -> Result<String, String> {
    let mem = ctx.memory.as_deref_mut().ok_or("memory system unavailable")?;
    let source = input["source"].as_str().ok_or("missing 'source' parameter")?;
    let chunk_size = input["chunk_size"].as_u64().unwrap_or(500) as usize;
    let chunk_overlap = input["chunk_overlap"].as_u64().unwrap_or(50) as usize;

    // Get text from either 'text' field or by reading 'file_path'.
    let text = if let Some(t) = input["text"].as_str() {
        t.to_string()
    } else if let Some(path) = input["file_path"].as_str() {
        let path = Path::new(path);
        if !path.exists() {
            return Err(format!("파일을 찾을 수 없습니다: {}", path.display()));
        }

        // Handle PDF via external script.
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext == "pdf" {
            // Try pdftotext (poppler), fallback to python.
            let output = Command::new("pdftotext")
                .args(["-", "-"])
                .arg(path.to_str().unwrap_or(""))
                .output()
                .await;

            match output {
                Ok(out) if out.status.success() => {
                    String::from_utf8_lossy(&out.stdout).to_string()
                }
                _ => {
                    // Fallback: python3 with PyPDF2 or pdfplumber.
                    let py_script = format!(
                        "import sys\ntry:\n  import pdfplumber\n  with pdfplumber.open('{}') as pdf:\n    for p in pdf.pages:\n      t = p.extract_text()\n      if t: print(t)\nexcept:\n  try:\n    from PyPDF2 import PdfReader\n    r = PdfReader('{}')\n    for p in r.pages: print(p.extract_text() or '')\n  except Exception as e:\n    print(f'Error: {{e}}', file=sys.stderr); sys.exit(1)",
                        path.display(), path.display()
                    );
                    let py_out = Command::new("python3")
                        .args(["-c", &py_script])
                        .output()
                        .await
                        .map_err(|e| format!("PDF extraction failed: {e}"))?;

                    if !py_out.status.success() {
                        return Err(format!(
                            "PDF 텍스트 추출 실패. pdfplumber 또는 PyPDF2를 설치해주세요: {}",
                            String::from_utf8_lossy(&py_out.stderr)
                        ));
                    }
                    String::from_utf8_lossy(&py_out.stdout).to_string()
                }
            }
        } else {
            // Plain text / markdown / code files.
            std::fs::read_to_string(path)
                .map_err(|e| format!("파일 읽기 실패: {e}"))?
        }
    } else {
        return Err("'text' 또는 'file_path' 중 하나를 제공해야 합니다.".to_string());
    };

    if text.trim().is_empty() {
        return Err("문서 내용이 비어있습니다.".to_string());
    }

    let char_count = text.chars().count();
    match mem.ingest_document(source, &text, chunk_size, chunk_overlap) {
        Ok(chunks) => Ok(format!(
            "문서 저장 완료: {source}\n- 원본 크기: {char_count}자\n- 청크 수: {chunks}개 (크기: {chunk_size}, 겹침: {chunk_overlap})\n이제 memory_recall로 이 문서의 내용을 검색할 수 있어."
        )),
        Err(e) => Err(format!("문서 저장 실패: {e}")),
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
        .map(sam_core::expand_tilde)
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
        .map(sam_core::expand_tilde)
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

    let max_turns = cc.max_turns.to_string();

    let result = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        Command::new(&binary)
            .arg("--print")
            .arg("--output-format")
            .arg("text")
            .arg("--max-turns")
            .arg(&max_turns)
            .arg("--permission-mode")
            .arg(&cc.default_permission_mode)
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

            // Detect the max-turns cap hit: Claude Code emits
            // "Error: Reached max turns (N)" to stdout and exits non-zero.
            // Surface it as a structured error so the LLM can react
            // instead of treating it like a normal reply.
            if code != 0 && stdout.contains("Reached max turns") {
                warn!(max_turns = cc.max_turns, "Claude Code hit max-turns cap");
                return Err(format!(
                    "Claude Code가 {turn}턴 한도에 도달했습니다. 작업이 완료되지 않았을 수 있으니 범위를 좁혀 다시 시도하거나 `[claude_code] max_turns` 값을 올리세요. 원본 출력: {out}",
                    turn = cc.max_turns,
                    out = truncate_output(&stdout, MAX_OUTPUT_BYTES / 4),
                ));
            }

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

// ── Twitter search ────────────────────────────────────────────────────

/// Load bearer token from the configured source.
fn load_twitter_bearer(config: &SamConfig) -> Result<String, String> {
    let source = &config.twitter.bearer_token_source;

    if let Some(var_name) = source.strip_prefix("env:") {
        return std::env::var(var_name)
            .map(|k| k.trim().to_string())
            .map_err(|_| format!("트위터 Bearer 토큰 환경변수 '{var_name}'이 설정되지 않았습니다"));
    }

    if let Some(file_path) = source.strip_prefix("file:") {
        let expanded = sam_core::expand_tilde(file_path);
        return std::fs::read_to_string(&expanded)
            .map(|k| k.trim().to_string())
            .map_err(|e| format!("트위터 Bearer 토큰 파일 읽기 실패 '{expanded}': {e}"));
    }

    Err("트위터 bearer_token_source가 올바르지 않습니다 (env: 또는 file: 접두사 필요)".to_string())
}

async fn exec_twitter_search(
    input: &serde_json::Value,
    ctx: &ToolContext<'_>,
) -> Result<String, String> {
    if !ctx.config.twitter.enabled {
        return Err("트위터 기능이 비활성화되어 있습니다. config.toml에서 [twitter] enabled = true로 설정하세요.".to_string());
    }

    let query = input["query"]
        .as_str()
        .ok_or("missing 'query' parameter")?;
    let max_results = input["max_results"]
        .as_u64()
        .unwrap_or(10)
        .clamp(10, 100);

    let bearer = load_twitter_bearer(ctx.config)?;

    info!(query = query, max_results = max_results, "twitter_search");

    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.twitter.com/2/tweets/search/recent")
        .header("authorization", format!("Bearer {bearer}"))
        .query(&[
            ("query", query),
            ("max_results", &max_results.to_string()),
            ("tweet.fields", "created_at,author_id,public_metrics,lang"),
            ("expansions", "author_id"),
            ("user.fields", "username,name"),
        ])
        .send()
        .await
        .map_err(|e| format!("트위터 API 요청 실패: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp
            .text()
            .await
            .unwrap_or_else(|_| "<body unreadable>".to_string());
        return Err(format!("트위터 API 에러 HTTP {status}: {body}"));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("트위터 응답 파싱 실패: {e}"))?;

    // Build username lookup from includes.users.
    let mut user_map = std::collections::HashMap::new();
    if let Some(users) = body["includes"]["users"].as_array() {
        for u in users {
            if let (Some(id), Some(username), Some(name)) = (
                u["id"].as_str(),
                u["username"].as_str(),
                u["name"].as_str(),
            ) {
                user_map.insert(id.to_string(), (username.to_string(), name.to_string()));
            }
        }
    }

    let tweets = body["data"]
        .as_array()
        .ok_or("검색 결과가 없습니다.")?;

    if tweets.is_empty() {
        return Ok("검색 결과가 없습니다.".to_string());
    }

    let mut result = format!("'{query}' 검색 결과 ({}건):\n\n", tweets.len());
    for (i, tweet) in tweets.iter().enumerate() {
        let text = tweet["text"].as_str().unwrap_or("");
        let author_id = tweet["author_id"].as_str().unwrap_or("unknown");
        let created = tweet["created_at"].as_str().unwrap_or("");

        let author_display = user_map
            .get(author_id)
            .map(|(username, name)| format!("{name} (@{username})"))
            .unwrap_or_else(|| author_id.to_string());

        let metrics = &tweet["public_metrics"];
        let likes = metrics["like_count"].as_u64().unwrap_or(0);
        let retweets = metrics["retweet_count"].as_u64().unwrap_or(0);
        let replies = metrics["reply_count"].as_u64().unwrap_or(0);

        result.push_str(&format!(
            "{}. {} ({})\n   {}\n   ❤️ {} 🔁 {} 💬 {}\n\n",
            i + 1,
            author_display,
            created,
            text.replace('\n', "\n   "),
            likes,
            retweets,
            replies,
        ));
    }

    Ok(truncate_output(&result, MAX_OUTPUT_BYTES))
}

// ── Web search ──────────────────────────────────────────────────────────

/// Load a secret from a source string (env:VAR or file:PATH).
/// Load an API key from a source specifier (env:VAR or file:/path).
/// Public alias for use by other modules in this crate.
pub fn load_key_from_source_pub(source: &str) -> Result<String, String> {
    load_key_from_source(source)
}

fn load_key_from_source(source: &str) -> Result<String, String> {
    if let Some(var_name) = source.strip_prefix("env:") {
        return std::env::var(var_name)
            .map(|k| k.trim().to_string())
            .map_err(|_| format!("환경변수 '{var_name}'이 설정되지 않았습니다"));
    }
    if let Some(file_path) = source.strip_prefix("file:") {
        let expanded = sam_core::expand_tilde(file_path);
        return std::fs::read_to_string(&expanded)
            .map(|k| k.trim().to_string())
            .map_err(|e| format!("파일 읽기 실패 '{expanded}': {e}"));
    }
    Err("소스 형식이 올바르지 않습니다 (env: 또는 file: 접두사 필요)".to_string())
}

async fn exec_web_search(
    input: &serde_json::Value,
    ctx: &ToolContext<'_>,
) -> Result<String, String> {
    if !ctx.config.web_search.enabled {
        return Err("웹 검색이 비활성화되어 있어. config.toml에서 [web_search] enabled = true로 설정해줘.".to_string());
    }

    let query = input["query"].as_str().ok_or("missing 'query' parameter")?;
    let max_results = input["max_results"].as_u64().unwrap_or(5).min(10);

    info!(query = query, max_results = max_results, provider = %ctx.config.web_search.provider, "web_search");

    match ctx.config.web_search.provider.as_str() {
        "brave" => exec_web_search_brave(query, max_results, ctx).await,
        _ => exec_web_search_xai(query, max_results, ctx).await,
    }
}

/// xAI Live Search — uses Grok's /v1/chat/completions with search tool.
async fn exec_web_search_xai(
    query: &str,
    max_results: u64,
    ctx: &ToolContext<'_>,
) -> Result<String, String> {
    // Resolve API key: use web_search.api_key_source if set, else fall back to llm.api_key_source.
    let api_key = if let Some(source) = &ctx.config.web_search.api_key_source {
        load_key_from_source(source)?
    } else if let Some(source) = &ctx.config.llm.api_key_source {
        load_key_from_source(source)?
    } else {
        return Err("xAI API 키가 설정되지 않았어.".to_string());
    };

    let base_url = &ctx.config.llm.base_url;

    let body = json!({
        "model": "grok-3-mini",
        "messages": [
            {
                "role": "system",
                "content": format!(
                    "You are a search assistant. Search the web for the query and return the top {} results. \
                     For each result, provide: title, URL, and a brief description. \
                     Format as a numbered list. Be concise. Respond in Korean if the query is in Korean.",
                    max_results
                )
            },
            {
                "role": "user",
                "content": query
            }
        ],
        "search_parameters": {
            "mode": "auto",
            "return_citations": true,
            "max_search_results": max_results
        },
        "temperature": 0.0,
        "max_tokens": 2048
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base_url}/v1/chat/completions"))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("xAI 검색 요청 실패: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let err_body = resp.text().await.unwrap_or_default();
        return Err(format!("xAI API 에러 HTTP {status}: {err_body}"));
    }

    let resp_body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("응답 파싱 실패: {e}"))?;

    // Extract the assistant's response text.
    let text = resp_body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("검색 결과를 가져오지 못했습니다.");

    // Also extract citations if available.
    let mut result = text.to_string();
    if let Some(citations) = resp_body["citations"].as_array() {
        if !citations.is_empty() {
            result.push_str("\n\n출처:\n");
            for (i, c) in citations.iter().enumerate() {
                if let Some(url) = c.as_str() {
                    result.push_str(&format!("[{}] {}\n", i + 1, url));
                }
            }
        }
    }

    Ok(truncate_output(&result, MAX_OUTPUT_BYTES))
}

/// Brave Search API fallback.
async fn exec_web_search_brave(
    query: &str,
    max_results: u64,
    ctx: &ToolContext<'_>,
) -> Result<String, String> {
    let api_key = ctx.config.web_search.api_key_source
        .as_deref()
        .ok_or("Brave Search API 키가 설정되지 않았어.")?;
    let api_key = load_key_from_source(api_key)?;

    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("X-Subscription-Token", &api_key)
        .header("Accept", "application/json")
        .query(&[
            ("q", query),
            ("count", &max_results.to_string()),
            ("search_lang", "ko"),
        ])
        .send()
        .await
        .map_err(|e| format!("검색 요청 실패: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Brave Search API 에러 HTTP {status}: {body}"));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("응답 파싱 실패: {e}"))?;

    let results = body["web"]["results"]
        .as_array()
        .ok_or("검색 결과가 없습니다.")?;

    if results.is_empty() {
        return Ok("검색 결과가 없습니다.".to_string());
    }

    let mut output = format!("'{query}' 검색 결과 ({}건):\n\n", results.len());
    for (i, r) in results.iter().enumerate() {
        let title = r["title"].as_str().unwrap_or("");
        let url = r["url"].as_str().unwrap_or("");
        let desc = r["description"].as_str().unwrap_or("");
        output.push_str(&format!("{}. {}\n   {}\n   {}\n\n", i + 1, title, url, desc));
    }

    Ok(truncate_output(&output, MAX_OUTPUT_BYTES))
}

// ── Cron/Reminder tools ──────────────────────────────────────────────────

async fn exec_schedule_reminder(
    input: &serde_json::Value,
    ctx: &ToolContext<'_>,
) -> Result<String, String> {
    let store_arc = ctx.cron_store.as_ref().ok_or("cron system unavailable")?;
    let message = input["message"].as_str().ok_or("missing 'message' parameter")?;

    let schedule = if let Some(cron_expr) = input["cron_expr"].as_str() {
        CronSchedule::Cron { expr: cron_expr.to_string() }
    } else if let Some(datetime) = input["datetime"].as_str() {
        let unix = parse_datetime_to_unix(datetime)
            .ok_or(format!("날짜 형식을 인식할 수 없어: '{datetime}'. 'YYYY-MM-DD HH:MM' 형식으로 입력해줘."))?;
        CronSchedule::Once { at_unix: unix }
    } else {
        return Err("'cron_expr' 또는 'datetime' 중 하나는 필수야.".to_string());
    };

    let repeat = input["repeat"].as_bool().unwrap_or(matches!(&schedule, CronSchedule::Cron { .. }));
    let job = new_job(&ctx.sender_handle, message, schedule.clone(), repeat);

    let mut store = store_arc.lock().await;
    let id = store.add(job);

    let schedule_desc = match &schedule {
        CronSchedule::Cron { expr } => format!("반복: {expr}"),
        CronSchedule::Once { at_unix } => {
            let dt = chrono::Local.timestamp_opt(*at_unix, 0)
                .single()
                .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| at_unix.to_string());
            format!("일회: {dt}")
        }
    };

    info!(id = %id, message = message, "reminder scheduled");
    Ok(format!("리마인더 예약 완료!\n내용: {message}\n스케줄: {schedule_desc}\nID: {id}"))
}

async fn exec_list_reminders(ctx: &ToolContext<'_>) -> Result<String, String> {
    let store_arc = ctx.cron_store.as_ref().ok_or("cron system unavailable")?;
    let store = store_arc.lock().await;
    let jobs = store.list();

    if jobs.is_empty() {
        return Ok("예약된 리마인더가 없어.".to_string());
    }

    let mut result = format!("예약된 리마인더 {}건:\n\n", jobs.len());
    for job in jobs {
        let schedule_desc = match &job.schedule {
            CronSchedule::Cron { expr } => format!("🔁 {expr}"),
            CronSchedule::Once { at_unix } => {
                let dt = chrono::Local.timestamp_opt(*at_unix, 0)
                    .single()
                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| at_unix.to_string());
                format!("📅 {dt}")
            }
        };
        result.push_str(&format!("• {} — {}\n  ID: {}\n", job.message, schedule_desc, job.id));
    }

    Ok(result)
}

async fn exec_cancel_reminder(
    input: &serde_json::Value,
    ctx: &ToolContext<'_>,
) -> Result<String, String> {
    let store_arc = ctx.cron_store.as_ref().ok_or("cron system unavailable")?;
    let mut store = store_arc.lock().await;

    if let Some(id) = input["id"].as_str() {
        if store.remove(id) {
            return Ok(format!("리마인더 삭제 완료 (ID: {id})"));
        } else {
            return Err(format!("ID '{id}'에 해당하는 리마인더를 찾지 못했어."));
        }
    }

    if let Some(query) = input["query"].as_str() {
        if let Some(job) = store.remove_by_message(query) {
            return Ok(format!("리마인더 삭제 완료: '{}'", job.message));
        } else {
            return Err(format!("'{query}'에 해당하는 리마인더를 찾지 못했어."));
        }
    }

    Err("'id' 또는 'query' 중 하나를 지정해줘.".to_string())
}

// ── Flow tools ───────────────────────────────────────────────────────────

async fn exec_run_flow(
    input: &serde_json::Value,
    ctx: &mut ToolContext<'_>,
) -> Result<String, String> {
    let flow_store_arc = ctx.flow_store.as_ref().ok_or("flow system unavailable")?.clone();
    let llm_client = ctx.llm_client.ok_or("LLM client unavailable for flow execution")?;
    let name = input["name"].as_str().ok_or("missing 'name' parameter")?;

    let flow = {
        let store = flow_store_arc.lock().await;
        store.get(name).cloned()
    };

    let flow = flow.ok_or(format!("플로우 '{name}'을 찾을 수 없어. list_flows로 목록을 확인해봐."))?;

    info!(flow = %flow.name, "running flow via tool");

    let result = run_flow(
        &flow,
        llm_client,
        ctx.memory.as_deref_mut(),
        ctx.config,
        ctx.cron_store.clone(),
        &ctx.sender_handle,
    )
    .await;

    if result.success {
        let mut output = format!("플로우 '{}' 완료!\n\n", flow.name);
        for step in &flow.steps {
            let step_name = step.step_name();
            if let Some(val) = result.outputs.get(step_name) {
                let truncated = if val.len() > 500 {
                    format!("{}...", &val[..500])
                } else {
                    val.clone()
                };
                output.push_str(&format!("[{step_name}] {truncated}\n"));
            }
        }
        Ok(output)
    } else {
        Err(result.error.unwrap_or_else(|| "unknown error".to_string()))
    }
}

async fn exec_list_flows(ctx: &ToolContext<'_>) -> Result<String, String> {
    let flow_store_arc = ctx.flow_store.as_ref().ok_or("flow system unavailable")?;
    let store = flow_store_arc.lock().await;
    let flows = store.list();

    if flows.is_empty() {
        return Ok("등록된 플로우가 없어. ~/.sam/flows/ 디렉토리에 TOML 파일을 추가해줘.".to_string());
    }

    let mut result = format!("등록된 플로우 {}건:\n\n", flows.len());
    for flow in flows {
        let trigger = match &flow.trigger {
            sam_core::FlowTrigger::Manual => "수동".to_string(),
            sam_core::FlowTrigger::Cron { expr } => format!("🔁 {expr}"),
        };
        result.push_str(&format!(
            "• {} — {} (트리거: {}, 단계 {}개)\n",
            flow.name, flow.description, trigger, flow.steps.len()
        ));
    }

    Ok(result)
}

// ── Notion ──────────────────────────────────────────────────────────────

async fn exec_notion_create_page(
    input: &serde_json::Value,
    ctx: &ToolContext<'_>,
) -> Result<String, String> {
    if !ctx.config.notion.enabled {
        return Err("Notion 기능이 비활성화되어 있어. config.toml에서 [notion] enabled = true로 설정해줘.".to_string());
    }

    let title = input["title"].as_str().ok_or("missing 'title' parameter")?;
    let content = input["content"].as_str().ok_or("missing 'content' parameter")?;

    let api_key = ctx.config.notion.api_key_source
        .as_deref()
        .ok_or("Notion API 키 소스가 설정되지 않았어.")?;
    let api_key = load_key_from_source(api_key)?;

    let parent_page_id = &ctx.config.notion.parent_page_id;
    if parent_page_id.is_empty() {
        return Err("Notion parent_page_id가 설정되지 않았어.".to_string());
    }

    // Build paragraph blocks from content (split by newlines).
    let children: Vec<serde_json::Value> = content
        .split('\n')
        .map(|line| {
            json!({
                "object": "block",
                "type": "paragraph",
                "paragraph": {
                    "rich_text": [{
                        "type": "text",
                        "text": { "content": line }
                    }]
                }
            })
        })
        .collect();

    let body = json!({
        "parent": {
            "page_id": parent_page_id
        },
        "properties": {
            "title": {
                "title": [{
                    "type": "text",
                    "text": { "content": title }
                }]
            }
        },
        "children": children
    });

    info!(title = title, parent = %parent_page_id, "notion_create_page");

    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.notion.com/v1/pages")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Notion-Version", "2022-06-28")
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Notion API 요청 실패: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let err_body = resp.text().await.unwrap_or_default();
        return Err(format!("Notion API 에러 HTTP {status}: {err_body}"));
    }

    let resp_body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Notion 응답 파싱 실패: {e}"))?;

    let url = resp_body["url"]
        .as_str()
        .unwrap_or("(URL 없음)");

    Ok(format!("Notion 페이지 생성 완료!\n제목: {title}\nURL: {url}"))
}

// ── Agent handoff ─────────────────────────────────────────────────────────

/// Returns a sentinel string that the router intercepts to switch agents.
/// The format is: `__HANDOFF__:<agent_name>:<context>`
fn exec_handoff(input: &serde_json::Value) -> Result<String, String> {
    let agent = input["agent"].as_str().ok_or("missing 'agent' parameter")?;
    let context = input["context"].as_str().unwrap_or("");
    Ok(format!("__HANDOFF__:{}:{}", agent, context))
}

// ── Transfer memo ─────────────────────────────────────────────────────────

/// Write a structured memo for agent handoff.
/// Stored as `~/.sam/state/memos/<sender>::<target>.json`.
fn exec_transfer_memo(input: &serde_json::Value, ctx: &ToolContext) -> Result<String, String> {
    let target = input["target_agent"]
        .as_str()
        .ok_or("missing 'target_agent'")?;
    let summary = input["summary"]
        .as_str()
        .ok_or("missing 'summary'")?;
    let key_facts: Vec<String> = input["key_facts"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let memo = serde_json::json!({
        "from_handle": ctx.sender_handle,
        "target_agent": target,
        "summary": summary,
        "key_facts": key_facts,
        "timestamp": chrono::Local::now().to_rfc3339(),
    });

    // Save to ~/.sam/state/memos/
    let memos_dir = sam_core::state_dir().join("memos");
    if let Err(e) = std::fs::create_dir_all(&memos_dir) {
        return Err(format!("failed to create memos directory: {e}"));
    }

    // Key: sanitize sender handle for filename.
    let sanitized_handle: String = ctx
        .sender_handle
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let filename = format!("{sanitized_handle}__{target}.json");
    let path = memos_dir.join(&filename);

    std::fs::write(&path, serde_json::to_string_pretty(&memo).unwrap_or_default())
        .map_err(|e| format!("failed to write memo: {e}"))?;

    Ok(format!("메모 저장 완료. {target} 에이전트가 인수인계 시 이 맥락을 받게 됩니다."))
}

/// Read a transfer memo left for a specific agent.
/// Returns None if no memo exists.
pub fn read_transfer_memo(sender_handle: &str, agent_name: &str) -> Option<String> {
    let memos_dir = sam_core::state_dir().join("memos");
    let sanitized_handle: String = sender_handle
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let filename = format!("{sanitized_handle}__{agent_name}.json");
    let path = memos_dir.join(&filename);

    match std::fs::read_to_string(&path) {
        Ok(data) => {
            // Delete after reading (one-time delivery).
            let _ = std::fs::remove_file(&path);
            if let Ok(memo) = serde_json::from_str::<serde_json::Value>(&data) {
                let mut result = String::new();
                if let Some(summary) = memo["summary"].as_str() {
                    result.push_str(summary);
                }
                if let Some(facts) = memo["key_facts"].as_array() {
                    if !facts.is_empty() {
                        result.push_str("\n\n핵심 사항:");
                        for fact in facts {
                            if let Some(f) = fact.as_str() {
                                result.push_str(&format!("\n- {f}"));
                            }
                        }
                    }
                }
                if result.is_empty() { None } else { Some(result) }
            } else {
                Some(data)
            }
        }
        Err(_) => None,
    }
}

// ── Browser automation ────────────────────────────────────────────────────

async fn exec_browser(
    input: &serde_json::Value,
    ctx: &ToolContext<'_>,
) -> Result<String, String> {
    if !ctx.config.browser.enabled {
        return Err("브라우저 기능이 비활성화되어 있어. config.toml에서 [browser] enabled = true로 설정해줘.".to_string());
    }

    let action = input["action"].as_str().ok_or("missing 'action' parameter")?;
    let url = input["url"].as_str().ok_or("missing 'url' parameter")?;
    let _selector = input["selector"].as_str().unwrap_or("body");

    let chrome_path = &ctx.config.browser.chrome_path;
    let timeout = ctx.config.browser.timeout_secs;
    let max_content = ctx.config.browser.max_content_bytes;

    // Decide execution mode: Chrome headless or curl fallback.
    let use_chrome = chrome_path != "curl"
        && !chrome_path.is_empty()
        && std::path::Path::new(chrome_path).exists();

    match action {
        "navigate" | "extract" => {
            let html = if use_chrome {
                let result = tokio::time::timeout(
                    Duration::from_secs(timeout),
                    Command::new(chrome_path)
                        .arg("--headless=new")
                        .arg("--disable-gpu")
                        .arg("--no-sandbox")
                        .arg("--dump-dom")
                        .arg(url)
                        .output(),
                ).await;
                match result {
                    Ok(Ok(output)) => String::from_utf8_lossy(&output.stdout).to_string(),
                    Ok(Err(e)) => return Err(format!("Chrome 실행 실패: {e}")),
                    Err(_) => return Err(format!("타임아웃: {timeout}초 초과")),
                }
            } else {
                // curl fallback.
                let result = tokio::time::timeout(
                    Duration::from_secs(timeout),
                    Command::new("curl")
                        .arg("-sL")
                        .arg("--max-time")
                        .arg(timeout.to_string())
                        .arg("-A")
                        .arg("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Sam/1.0")
                        .arg(url)
                        .output(),
                ).await;
                match result {
                    Ok(Ok(output)) => String::from_utf8_lossy(&output.stdout).to_string(),
                    Ok(Err(e)) => return Err(format!("curl 실행 실패: {e}")),
                    Err(_) => return Err(format!("타임아웃: {timeout}초 초과")),
                }
            };

            let text = strip_html_tags(&html);
            let truncated = if text.len() > max_content {
                let mut end = max_content;
                while end > 0 && !text.is_char_boundary(end) { end -= 1; }
                format!("{}...\n[{max_content}바이트로 잘림]", &text[..end])
            } else {
                text
            };
            Ok(format!("[{url}]\n\n{truncated}"))
        }
        "screenshot" => {
            if !use_chrome {
                return Err("스크린샷은 Chrome/Chromium이 필요해. config.toml [browser] chrome_path를 설정해줘.".to_string());
            }
            let output_path = format!("/tmp/sam_browser_{}.png", std::process::id());
            let result = tokio::time::timeout(
                Duration::from_secs(timeout),
                Command::new(chrome_path)
                    .arg("--headless=new")
                    .arg("--disable-gpu")
                    .arg("--no-sandbox")
                    .arg(format!("--screenshot={output_path}"))
                    .arg("--window-size=1280,900")
                    .arg(url)
                    .output(),
            ).await;
            match result {
                Ok(Ok(output)) => {
                    if output.status.success() && std::path::Path::new(&output_path).exists() {
                        Ok(format!("스크린샷 저장됨: {output_path}"))
                    } else {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        Err(format!("스크린샷 실패: {stderr}"))
                    }
                }
                Ok(Err(e)) => Err(format!("Chrome 실행 실패: {e}")),
                Err(_) => Err(format!("타임아웃: {timeout}초 초과")),
            }
        }
        _ => Err(format!("unknown browser action: {action}. Use: navigate, screenshot, extract")),
    }
}

/// Simple HTML tag stripper for text extraction.
fn strip_html_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut in_script = false;
    let mut in_style = false;
    let lower = html.to_lowercase();
    let chars: Vec<char> = html.chars().collect();
    let lower_chars: Vec<char> = lower.chars().collect();

    let mut i = 0;
    while i < chars.len() {
        if !in_tag && chars[i] == '<' {
            in_tag = true;
            // Check for script/style start
            let remaining: String = lower_chars[i..].iter().take(10).collect();
            if remaining.starts_with("<script") {
                in_script = true;
            } else if remaining.starts_with("<style") {
                in_style = true;
            } else if remaining.starts_with("</script") {
                in_script = false;
            } else if remaining.starts_with("</style") {
                in_style = false;
            }
        } else if in_tag && chars[i] == '>' {
            in_tag = false;
        } else if !in_tag && !in_script && !in_style {
            result.push(chars[i]);
        }
        i += 1;
    }

    // Collapse whitespace.
    let mut collapsed = String::with_capacity(result.len());
    let mut prev_space = false;
    for ch in result.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                collapsed.push(' ');
                prev_space = true;
            }
        } else {
            collapsed.push(ch);
            prev_space = false;
        }
    }
    collapsed.trim().to_string()
}

// ── Custom skill execution ────────────────────────────────────────────────

async fn execute_custom_skill(
    skill: &sam_core::CustomSkill,
    input: &serde_json::Value,
) -> Result<String, String> {
    let args: Vec<String> = skill
        .exec
        .args
        .iter()
        .map(|a| interpolate_args(a, input))
        .collect();

    let mut cmd = Command::new(&skill.exec.command);
    cmd.args(&args);

    // Set optional environment variables.
    for (k, v) in &skill.exec.env {
        cmd.env(k, v);
    }

    let timeout = skill.exec.timeout_secs;

    let result = tokio::time::timeout(Duration::from_secs(timeout), cmd.output()).await;

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
        Ok(Err(e)) => Err(format!("skill command execution failed: {e}")),
        Err(_) => Err(format!("skill timeout: {timeout}s exceeded")),
    }
}

// ── Image generation (DALL-E API) ─────────────────────────────────────

/// Generate an image via OpenAI DALL-E API. Returns a special
/// `__ATTACHMENT__:/path/to/image.png` marker that the daemon intercepts.
async fn exec_generate_image(
    input: &serde_json::Value,
    _ctx: &ToolContext<'_>,
) -> Result<String, String> {
    let prompt = input["prompt"].as_str().ok_or("prompt is required")?;
    let size = input["size"].as_str().unwrap_or("1024x1024");
    let style = input["style"].as_str().unwrap_or("vivid");

    // Read OpenAI API key from config or env.
    let api_key = std::env::var("OPENAI_API_KEY")
        .or_else(|_| {
            let key_path = sam_core::sam_home().join("openai_api_key");
            std::fs::read_to_string(key_path).map(|s| s.trim().to_string())
        })
        .map_err(|_| "OPENAI_API_KEY not set and ~/.sam/openai_api_key not found".to_string())?;

    let output_dir = sam_core::state_dir().join("generated");
    std::fs::create_dir_all(&output_dir)
        .map_err(|e| format!("failed to create output dir: {e}"))?;

    let filename = format!("img_{}.png", chrono::Utc::now().timestamp_millis());
    let output_path = output_dir.join(&filename);

    // Call DALL-E API.
    let body = json!({
        "model": "dall-e-3",
        "prompt": prompt,
        "n": 1,
        "size": size,
        "style": style,
        "response_format": "url"
    });

    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.openai.com/v1/images/generations")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("DALL-E API request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("DALL-E API error {status}: {text}"));
    }

    let resp_json: serde_json::Value = resp.json().await
        .map_err(|e| format!("failed to parse DALL-E response: {e}"))?;

    let image_url = resp_json["data"][0]["url"].as_str()
        .ok_or("no URL in DALL-E response")?;

    // Download the image.
    let image_bytes = client.get(image_url).send().await
        .map_err(|e| format!("failed to download image: {e}"))?
        .bytes().await
        .map_err(|e| format!("failed to read image bytes: {e}"))?;

    std::fs::write(&output_path, &image_bytes)
        .map_err(|e| format!("failed to save image: {e}"))?;

    let path_str = output_path.to_string_lossy().to_string();
    info!(path = %path_str, prompt = %prompt, "image generated");

    Ok(format!("__ATTACHMENT__:{path_str}\n이미지를 생성했어. ({size}, {style})"))
}

// ── Chart generation (matplotlib) ─────────────────────────────────────

/// Generate a chart via matplotlib. Returns `__ATTACHMENT__:/path` marker.
async fn exec_generate_chart(
    input: &serde_json::Value,
    _ctx: &ToolContext<'_>,
) -> Result<String, String> {
    let title = input["title"].as_str().unwrap_or("Chart");
    let chart_type = input["chart_type"].as_str().unwrap_or("bar");

    let output_dir = sam_core::state_dir().join("generated");
    std::fs::create_dir_all(&output_dir)
        .map_err(|e| format!("failed to create output dir: {e}"))?;

    let filename = format!("chart_{}.png", chrono::Utc::now().timestamp_millis());
    let output_path = output_dir.join(&filename);
    let output_path_str = output_path.to_string_lossy().to_string();

    // If custom python_code is provided, use it directly.
    let python_code = if let Some(code) = input["python_code"].as_str() {
        code.replace("output_path", &format!("'{output_path_str}'"))
    } else {
        // Build matplotlib code from structured data.
        let labels: Vec<String> = input["data"]["labels"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();

        let values_json = &input["data"]["values"];

        let data_code = if let Some(arr) = values_json.as_array() {
            // Simple array of numbers.
            let vals: Vec<f64> = arr.iter().filter_map(|v| v.as_f64()).collect();
            format!("labels = {:?}\nvalues = {:?}\n", labels, vals)
        } else if let Some(obj) = values_json.as_object() {
            // Multi-series: {name: [values]}
            let mut code = format!("labels = {:?}\n", labels);
            for (name, vals) in obj {
                let nums: Vec<f64> = vals.as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_f64()).collect())
                    .unwrap_or_default();
                code.push_str(&format!("{name} = {:?}\n", nums));
            }
            code
        } else {
            "labels = []\nvalues = []\n".to_string()
        };

        let plot_code = match chart_type {
            "pie" => "plt.pie(values, labels=labels, autopct='%1.1f%%')\n".to_string(),
            "line" => "plt.plot(labels, values, marker='o')\n".to_string(),
            "scatter" => "plt.scatter(range(len(values)), values)\nplt.xticks(range(len(labels)), labels)\n".to_string(),
            "histogram" => "plt.hist(values, bins='auto', edgecolor='black')\n".to_string(),
            _ => "plt.bar(labels, values)\n".to_string(), // default: bar
        };

        format!(
            "import matplotlib\nmatplotlib.use('Agg')\nimport matplotlib.pyplot as plt\n\
             plt.rcParams['font.family'] = 'AppleGothic'\n\
             plt.rcParams['axes.unicode_minus'] = False\n\
             {data_code}\n\
             plt.figure(figsize=(10, 6))\n\
             {plot_code}\
             plt.title('{title}')\n\
             plt.tight_layout()\n\
             plt.savefig('{output_path_str}', dpi=150, bbox_inches='tight')\n\
             plt.close()\n\
             print('ok')\n"
        )
    };

    // Write script to temp file and execute.
    let script_path = output_dir.join("_chart_script.py");
    std::fs::write(&script_path, &python_code)
        .map_err(|e| format!("failed to write chart script: {e}"))?;

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new("python3").arg(&script_path).output(),
    )
    .await;

    // Clean up script.
    let _ = std::fs::remove_file(&script_path);

    match result {
        Ok(Ok(output)) => {
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("chart generation failed: {stderr}"));
            }
            if !output_path.exists() {
                return Err("chart file was not created".to_string());
            }
            info!(path = %output_path_str, chart_type = %chart_type, "chart generated");
            Ok(format!("__ATTACHMENT__:{output_path_str}\n차트를 생성했어. ({chart_type}: {title})"))
        }
        Ok(Err(e)) => Err(format!("python3 execution failed: {e}")),
        Err(_) => Err("chart generation timeout (30s)".to_string()),
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
        assert_eq!(defs.len(), 21);
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
        let mut ctx = ToolContext { memory: None, config: &config, cron_store: None, sender_handle: String::new(), flow_store: None, llm_client: None, mcp_clients: None, skill_store: None };
        let input = json!({});
        let result = rt.block_on(execute_builtin("nonexistent", &input, &mut ctx));
        assert!(result.is_err());
    }

    #[test]
    fn memory_recall_without_adapter_returns_error() {
        let config = SamConfig::default();
        let mut ctx = ToolContext { memory: None, config: &config, cron_store: None, sender_handle: String::new(), flow_store: None, llm_client: None, mcp_clients: None, skill_store: None };
        let input = json!({"query": "test"});
        let result = exec_memory_recall(&input, &mut ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unavailable"));
    }

    #[test]
    fn memory_store_without_adapter_returns_error() {
        let config = SamConfig::default();
        let mut ctx = ToolContext { memory: None, config: &config, cron_store: None, sender_handle: String::new(), flow_store: None, llm_client: None, mcp_clients: None, skill_store: None };
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
        let ctx = ToolContext { memory: None, config: &config, cron_store: None, sender_handle: String::new(), flow_store: None, llm_client: None, mcp_clients: None, skill_store: None };
        let input = json!({"command": "echo hello", "working_dir": "/tmp"});
        let result = rt.block_on(exec_run_command(&input, &ctx));
        assert!(result.is_ok());
        assert!(result.unwrap().contains("hello"));
    }

    #[test]
    fn run_command_blocked_by_safety() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut config = SamConfig::default();
        config.safety.destructive_patterns = vec!["rm -rf".to_string()];
        let ctx = ToolContext { memory: None, config: &config, cron_store: None, sender_handle: String::new(), flow_store: None, llm_client: None, mcp_clients: None, skill_store: None };
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
