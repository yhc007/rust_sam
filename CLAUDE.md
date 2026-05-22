# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

Sam is a personal AI agent accessible via iMessage, Telegram, and a web chat UI. It runs as a macOS daemon that polls the iMessage database, routes messages through LLM APIs with an agentic tool-use loop, and responds via AppleScript. The web chat (`sam-agent web`) works on any platform including Linux.

## Build & Test

```bash
cargo build -p sam-agent                          # Debug build
cargo build -p sam-agent --release                # Release build
cargo test --workspace                            # All tests
cargo test -p sam-claude                          # Single crate
cargo test -p sam-claude -- deserialize_text      # Single test by name
cargo clippy --workspace -- -D warnings           # Lint (treat warnings as errors)

# Run daemon (requires ~/.sam/config.toml + API key)
SAM_LOG=info cargo run -p sam-agent -- daemon

# Run web chat only (no iMessage dependency, works on Linux)
cargo run -p sam-agent -- web --port 3547

# Other subcommands: status, chat, telegram, dashboard, send, import-memories
```

## Architecture

Cargo workspace: 5 library crates + 1 binary service + 1 vendored submodule.

```
sam-agent (binary — daemon, web, chat, telegram, etc.)
├── sam-claude       ← LLM clients, ConversationSession, tool execution, flows
│   ├── sam-core     ← SamConfig, paths, agents, flows, skills, cron (leaf crate)
│   └── sam-memory-adapter  ← wrapper around memory-brain MemoryGuardian
│       └── memory-actor (vendor/memory-brain)
├── sam-imessage     ← macOS-only: chat.db poller (rusqlite), osascript sender
│   └── sam-core
└── sam-tools        ← external tool registry (~/.sam/tools/*.toml)
    └── sam-core
```

### Daemon Task Architecture

Three concurrent tokio tasks connected via `mpsc` channels:

1. **Poller** (`sam-imessage`) — reads `~/Library/Messages/chat.db` every `poll_interval_ms`, filters by `allowed_handles`, emits `IncomingMessage`
2. **Router** (`sam-agent/cmd/daemon.rs`) — receives messages, maintains per-handle `ConversationSession`, calls LLM with agentic tool loop (max `MAX_TOOL_ROUNDS=30` iterations), handles memory recall/store, fallback LLM on failure
3. **Sender** (`sam-imessage`) — rate-limited queue (`send_rate_limit_ms`), dispatches via osascript

Plus optional tasks: web server, cron scheduler, heartbeat, hot-reload watcher.

### LLM Provider Abstraction

Trait `LlmBackend: Send + Sync` with a single async `chat()` method. Three implementations:

| Provider | Client | Wire Format |
|----------|--------|-------------|
| `anthropic` (default) | `SamClaudeClient` | Claude Messages API |
| `xai` | `XaiClient` | OpenAI-compatible |
| `openai-compatible` | `OpenAiCompatibleClient` | OpenAI chat completions (vLLM, local models) |

Selected by `config.llm.provider`. Fallback client (`config.llm.fallback`) is tried when primary fails. Fast client (`config.llm.fast`) used for trivial messages.

### Agentic Tool Loop (`session.rs`)

1. User message → append to history → call LLM with system prompt + history + tool definitions
2. If `stop_reason == "tool_use"` → execute each tool via `execute_builtin()` → append `tool_result` → re-call LLM
3. Repeat up to `MAX_TOOL_ROUNDS` (30). Same tool capped at `MAX_SAME_TOOL_CALLS` (15) per turn.
4. On `stop_reason == "end_turn"` → return final text, auto-store conversation to memory.

Context compaction: when history tokens exceed `max_context_tokens` (default 16,000), oldest messages are summarized into `context_summary` and dropped.

### Built-in Tools (15)

Defined in `sam-claude/src/tools.rs`. Key ones:
- `memory_recall` / `memory_store` — long-term memory via MemoryAdapter
- `run_command` — shell execution with safety filtering, 120s default timeout, 600s max
- `claude_code` — spawns Claude Code CLI (`--print` mode), supports local and remote (SSH) execution
- `read_file` / `write_file` — file I/O with line limits and auto-mkdir
- `web_search` — Tavily or xAI search provider
- `schedule_reminder` / `list_reminders` / `cancel_reminder` — cron-based scheduling
- `handoff_to_agent` — transfer conversation to another agent with context
- `notion_create_page` — Notion API integration

External tools loaded from `~/.sam/tools/*.toml` (SkillStore) and MCP servers (`[mcp]` config).

### Web Chat (`cmd/web.rs`)

Axum HTTP server with REST API, token-based auth (HttpOnly cookie), multi-session support.

Key endpoints:
- `/api/login`, `/api/logout`, `/api/me` — auth
- `/api/sessions` (GET/POST), `/api/sessions/{id}` (DELETE/PATCH) — session CRUD
- `/api/chat` — send message, get reply (uses same `session.reply()` as iMessage)
- `/api/agents` — list available agents
- `/api/memory`, `/api/dream` — memory stats and consolidation
- `/api/claude-status` — active tool execution status (for real-time UI)
- `/api/map` — knowledge graph (SPO triple extraction via LLM)

Frontend is a single `static/chat.html` (inline CSS/JS) embedded via `include_str!`.

### Memory System

`MemoryAdapter` wraps `memory-brain`'s `MemoryGuardian` (vendored submodule). BGE-M3 HTTP embeddings with hash-based fallback when embedder is unreachable. Persisted to `~/.sam/data/memories.json`.

Auto-recall injects relevant memories into system prompt before each LLM call. Auto-store saves conversation after each response. Tools `memory_recall`/`memory_store` allow explicit access.

## Configuration

`~/.sam/config.toml` — main config with sections: `[identity]`, `[imessage]`, `[llm]` (+ `[llm.fallback]`, `[llm.fast]`), `[memory]`, `[claude_code]`, `[safety]`, `[notion]`, `[telegram]`, `[web_search]`, `[whisper]`, `[heartbeat]`, `[mcp]`, `[agents]`, `[browser]`, `[web_chat]`.

API keys: `api_key_source = "file:~/.sam/anthropic_key"` or `"env:VAR_NAME"`.

System prompt: `~/.sam/prompts/system.txt`. Per-agent prompts: `~/.sam/prompts/{name}.md`.

Agents: `~/.sam/agents/*.toml`. Flows: `~/.sam/flows/*.toml`. Skills: `~/.sam/tools/*.toml`.

## Key Conventions

- **Error handling:** `thiserror` in library crates, `anyhow` in sam-agent. No `unwrap()`/`panic!()` in production paths — propagate with `?`.
- **Async:** all I/O-bound work is async (tokio). CPU-bound parsing/formatting stays sync.
- **Korean:** user-facing messages (error strings, system prompts) are in Korean. Code identifiers and comments in English.
- **Config hot-reload:** `~/.sam/config.toml` and `~/.sam/flows/` are polled for changes; no daemon restart needed for most config changes.
- **Safety:** destructive patterns in `[safety].destructive_patterns` are blocked by `run_command`. Claude Code runs with `--print` + configured permission mode.

## Submodule

`vendor/memory-brain/` is a git submodule. Clone with `--recurse-submodules` or run `git submodule update --init --recursive`.
