//! Integration tests for multi-agent routing and slash commands.

use sam_core::{AgentDef, AgentStore, ToolFilter};
use std::collections::BTreeMap;

#[test]
fn keyword_classification_routes_correctly() {
    let mut agents = BTreeMap::new();
    agents.insert(
        "coder".to_string(),
        AgentDef {
            name: "coder".to_string(),
            description: "코딩 작업".to_string(),
            prompt_file: "coder.md".to_string(),
            tools: ToolFilter::All,
            triggers: vec![
                "코드".to_string(),
                "code".to_string(),
                "빌드".to_string(),
                "build".to_string(),
                "컴파일".to_string(),
            ],
            can_handoff_to: vec!["default".to_string()],
            priority: 10,
        },
    );
    agents.insert(
        "scheduler".to_string(),
        AgentDef {
            name: "scheduler".to_string(),
            description: "일정 관리".to_string(),
            prompt_file: "scheduler.md".to_string(),
            tools: ToolFilter::Allow {
                names: vec![
                    "schedule_reminder".to_string(),
                    "list_reminders".to_string(),
                    "cancel_reminder".to_string(),
                    "current_time".to_string(),
                ],
            },
            triggers: vec![
                "리마인더".to_string(),
                "알림".to_string(),
                "일정".to_string(),
                "예약".to_string(),
            ],
            can_handoff_to: vec!["default".to_string()],
            priority: 5,
        },
    );
    agents.insert(
        "researcher".to_string(),
        AgentDef {
            name: "researcher".to_string(),
            description: "검색 및 조사".to_string(),
            prompt_file: "researcher.md".to_string(),
            tools: ToolFilter::Deny {
                names: vec!["write_file".to_string(), "run_command".to_string()],
            },
            triggers: vec![
                "검색".to_string(),
                "찾아".to_string(),
                "조사".to_string(),
            ],
            can_handoff_to: vec!["coder".to_string()],
            priority: 5,
        },
    );

    let store = AgentStore::from_map(agents);

    // Keyword matches.
    assert_eq!(store.classify("이 코드 리뷰해줘"), Some("coder"));
    assert_eq!(store.classify("내일 9시 알림 설정해"), Some("scheduler"));
    assert_eq!(store.classify("이것 좀 검색해줘"), Some("researcher"));

    // No match.
    assert_eq!(store.classify("안녕하세요"), None);
    assert_eq!(store.classify("날씨 어때?"), None);

    // Priority: "코드 검색" has both coder(10) and researcher(5) triggers → coder wins.
    assert_eq!(store.classify("코드 검색해줘"), Some("coder"));
}

#[test]
fn tool_filter_allow_restricts_tools() {
    let filter = ToolFilter::Allow {
        names: vec![
            "current_time".to_string(),
            "schedule_reminder".to_string(),
        ],
    };

    assert!(filter.allows("current_time"));
    assert!(filter.allows("schedule_reminder"));
    assert!(!filter.allows("run_command"));
    assert!(!filter.allows("write_file"));
    assert!(!filter.allows("claude_code"));
}

#[test]
fn tool_filter_deny_excludes_tools() {
    let filter = ToolFilter::Deny {
        names: vec!["run_command".to_string(), "write_file".to_string()],
    };

    assert!(filter.allows("current_time"));
    assert!(filter.allows("memory_recall"));
    assert!(!filter.allows("run_command"));
    assert!(!filter.allows("write_file"));
}

#[test]
fn llm_classify_prompt_contains_all_agents() {
    let mut agents = BTreeMap::new();
    agents.insert(
        "a".to_string(),
        AgentDef {
            name: "a".to_string(),
            description: "Agent A does things".to_string(),
            prompt_file: "a.md".to_string(),
            tools: ToolFilter::All,
            triggers: vec![],
            can_handoff_to: vec![],
            priority: 0,
        },
    );
    agents.insert(
        "b".to_string(),
        AgentDef {
            name: "b".to_string(),
            description: "Agent B does other things".to_string(),
            prompt_file: "b.md".to_string(),
            tools: ToolFilter::All,
            triggers: vec![],
            can_handoff_to: vec![],
            priority: 0,
        },
    );

    let store = AgentStore::from_map(agents);
    let (_sys, prompt) = store.build_classify_prompt("hello world", "default");

    assert!(prompt.contains("Agent A does things"));
    assert!(prompt.contains("Agent B does other things"));
    assert!(prompt.contains("hello world"));
    assert!(prompt.contains("default"));
}

#[test]
fn parse_classify_response_handles_edge_cases() {
    let mut agents = BTreeMap::new();
    agents.insert(
        "coder".to_string(),
        AgentDef {
            name: "coder".to_string(),
            description: "code".to_string(),
            prompt_file: "coder.md".to_string(),
            tools: ToolFilter::All,
            triggers: vec![],
            can_handoff_to: vec![],
            priority: 0,
        },
    );
    let store = AgentStore::from_map(agents);

    // Exact match.
    assert_eq!(store.parse_classify_response("coder", "default"), "coder");
    // With whitespace.
    assert_eq!(store.parse_classify_response("  coder  \n", "default"), "coder");
    // Case insensitive.
    assert_eq!(store.parse_classify_response("CODER", "default"), "coder");
    // Unknown → default.
    assert_eq!(store.parse_classify_response("unknown_agent", "default"), "default");
    // Empty → default.
    assert_eq!(store.parse_classify_response("", "default"), "default");
}
