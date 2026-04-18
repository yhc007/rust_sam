# Sam — Personal AI Agent via iMessage

## Overview

Sam은 iMessage를 통해 대화하는 개인 AI 에이전트다. Claude API를 사용하며, 도구 호출(tool_use)을 통해 쉘 명령 실행, 파일 관리, Claude Code 연동까지 가능하다. macOS LaunchAgent로 상시 실행되며, 크래시 시 자동 재시작된다.

## Architecture

```
iPhone (iMessage)
    │
    ▼
┌─────────────────────────────────────────────┐
│  sam-agent daemon (LaunchAgent)              │
│                                              │
│  ┌──────────┐  ┌───────────┐  ┌──────────┐  │
│  │ Poller   │→ │ Router    │→ │ Sender   │  │
│  │(chat.db) │  │(sessions) │  │(osascript)│  │
│  └──────────┘  └─────┬─────┘  └──────────┘  │
│                      │                       │
│            ┌─────────┴─────────┐             │
│            ▼                   ▼             │
│     ┌────────────┐     ┌────────────┐        │
│     │ Claude API │     │   Tools    │        │
│     │ (tool_use) │     │            │        │
│     └────────────┘     │ memory_*   │        │
│                        │ run_command│        │
│                        │ read/write │        │
│                        │ claude_code│        │
│                        └─────┬──────┘        │
│                              │               │
│                    ┌─────────┴──────┐        │
│                    ▼                ▼        │
│              ┌──────────┐   ┌────────────┐   │
│              │ Memory   │   │ Claude Code │   │
│              │ Brain    │   │ CLI (--print)│  │
│              └──────────┘   └────────────┘   │
└─────────────────────────────────────────────┘
```

## Milestones

### M1: Project Skeleton
- Cargo workspace 구조 설계
- 5개 crate + 1개 service 생성
- `sam-core`: config, paths, error types
- `sam-imessage`: chat.db poller, osascript sender
- `sam-claude`: Claude API client stub
- `sam-tools`: external tool registry
- `sam-memory-adapter`: memory-brain adapter
- `sam-agent`: CLI binary (daemon, status, send)

### M2: iMessage Integration
- chat.db SQLite polling (rusqlite, read-only)
- AppleScript sender (osascript)
- 메시지 수신 → 처리 → 응답 파이프라인
- 핸들 화이트리스트 기반 필터링
- echo dedup (자기 메시지 무시)
- UTF-8 안전 메시지 분할 (한글 지원)

### M3: Claude API Direct Integration
- Claude Messages API 직접 호출 (reqwest)
- 429/5xx 자동 재시도 (exponential backoff)
- 일일 토큰 예산 관리 (자동 리셋)
- API key 로드 (env var / file)

### M4: Memory Auto-Store
- memory-brain 연동 (MemoryAdapter)
- 대화 자동 저장
- BGE-M3 임베딩 또는 hash fallback

### M5: Claude Tool Use
- tool_use API 지원 (ContentBlock, ToolCall 타입)
- Agentic loop (최대 10회 반복)
- 내장 도구 3개: memory_recall, memory_store, current_time
- system prompt에 도구 가이드 추가

### M6: Stabilization
- 에러 핸들링 강화
  - `SamClaudeClient::new()` → Result (panic 제거)
  - API key 에러 메시지에 소스명 포함
  - 응답 body 읽기 실패 시 컨텍스트 보존
- 테스트 확장 (63개 → 70개)
- GitHub Actions CI (build + test + clippy)

### M7: LaunchAgent Deployment
- `com.sam.agent.plist` — 로그인 시 자동 시작
- 크래시 시 20초 후 자동 재시작
- 로그: `~/.sam/logs/sam-agent.{log,err}`
- install.sh / uninstall.sh / log-rotate.sh

### M8: Execution Tools
- `run_command` — 쉘 명령 실행 (safety 체크)
- `read_file` — 파일 읽기 (줄 수 제한)
- `write_file` — 파일 생성/수정 (auto-mkdir)
- `claude_code` — Claude Code CLI 연동 (--print 모드)
- 기본 작업 디렉토리: `/Volumes/T7/Sam`
- 파괴적 명령 자동 차단 (rm -rf, sudo 등)

## Crate Structure

| Crate | Description | Dependencies |
|-------|-------------|-------------|
| `sam-core` | Config, paths, error types | serde, toml, dirs |
| `sam-imessage` | iMessage poller & sender | sam-core, rusqlite, tokio |
| `sam-claude` | Claude API + tools + session | sam-core, sam-memory-adapter, reqwest, tokio |
| `sam-tools` | External tool registry (TOML) | sam-core, walkdir |
| `sam-memory-adapter` | memory-brain adapter | sam-core, memory-actor |
| `sam-agent` | Daemon binary | all above |

## Configuration

**파일:** `~/.sam/config.toml`

```toml
[identity]
name = "Sam"
owner = "Paul"

[imessage]
allowed_handles = ["+821038600983"]
poll_interval_ms = 1000

[llm]
model = "claude-sonnet-4-20250514"
max_tokens = 4096
daily_token_budget = 1_000_000
api_key_source = "file:~/.sam/anthropic_key"

[memory]
embedder_url = "http://localhost:3200"

[claude_code]
binary = "~/.local/bin/claude"
hard_timeout_secs = 7200

[safety]
destructive_patterns = ["rm -rf", "sudo", "git push --force", ...]
```

## Available Tools (7개)

| Tool | Description | Example |
|------|-------------|---------|
| `memory_recall` | 장기 기억 검색 | "치과 예약 언제였지?" |
| `memory_store` | 중요 정보 저장 | "이거 기억해: 내일 미팅 3시" |
| `current_time` | 현재 시간 확인 | "지금 몇 시야?" |
| `run_command` | 쉘 명령 실행 | "~/work에 뭐가 있어?" |
| `read_file` | 파일 내용 읽기 | "package.json 보여줘" |
| `write_file` | 파일 생성/수정 | "hello.py 만들어줘" |
| `claude_code` | 코딩 작업 위임 | "React 프로젝트 만들어줘" |

## Safety

- 파괴적 명령 패턴 매칭 → 자동 차단
- 차단 패턴: `rm -rf`, `git reset --hard`, `git push --force`, `DROP TABLE`, `sudo`, `shutdown`, `killall`
- Claude Code: `--print` 모드 (비대화형), 타임아웃 2시간

## Operations

```bash
# 상태 확인
launchctl list | grep com.sam.agent

# 로그 보기
tail -f ~/.sam/logs/sam-agent.err

# 재시작
launchctl unload ~/Library/LaunchAgents/com.sam.agent.plist
launchctl load ~/Library/LaunchAgents/com.sam.agent.plist

# 수동 실행
SAM_LOG=info sam-agent daemon

# 테스트
cargo test --workspace
```

## Repository

- **GitHub:** https://github.com/yhc007/rust_sam
- **Memory Brain (submodule):** https://github.com/yhc007/memory-brain
- **CI:** GitHub Actions (macOS, build + test + clippy)

## Test Coverage

- **70개 테스트** 전부 통과
- sam-core: 4 tests (config parsing, tilde expansion)
- sam-imessage: 8 tests (reader, sender, state)
- sam-claude: 26 tests (API types, budget, tools, session)
- sam-tools: 4 tests (registry scan, duplicates)
- sam-memory-adapter: 1 test (store/recall roundtrip)
- sam-agent: 4 tests (message splitting)
- memory-actor: 21 tests + 2 ignored
