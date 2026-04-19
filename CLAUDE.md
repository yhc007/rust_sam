# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

Sam is a personal AI agent that communicates via iMessage, powered by the Claude API. It runs as a macOS LaunchAgent daemon that polls the iMessage database, routes messages through Claude with tool_use, and sends responses back via AppleScript.

## Build & Test Commands

```bash
cargo build -p sam-agent                # Build the daemon binary
cargo build -p sam-agent --release      # Release build
cargo test --workspace                  # Run all ~70 tests
cargo test -p sam-claude                # Test a single crate
cargo test -p sam-claude -- test_name   # Run a single test
SAM_LOG=info cargo run -p sam-agent -- daemon   # Run the daemon
cargo clippy --workspace               # Lint
```

## Architecture

The system is a Cargo workspace with 5 library crates and 1 binary service:

**Dependency graph:**
```
sam-agent (binary)
├── sam-claude       ← Claude API client, ConversationSession, tool execution
│   ├── sam-core     ← SamConfig, paths, error types (leaf crate)
│   └── sam-memory-adapter  ← adapter to memory-brain (vendor submodule)
│       └── sam-core
├── sam-imessage     ← chat.db poller (rusqlite), osascript sender
│   └── sam-core
└── sam-tools        ← external tool registry (~/.sam/tools/*.toml)
    └── sam-core
```

**Three concurrent tokio tasks in the daemon:**
1. **Poller** — reads chat.db every 1s, filters by allowed_handles, sends `IncomingMessage` to channel
2. **Router** — receives messages, manages per-handle `ConversationSession`, calls Claude API with agentic tool loop (max 10 iterations), stores to memory
3. **Sender** — rate-limited queue (300ms/msg), sends via osascript

**Key types:**
- `SamConfig` (sam-core) — root config loaded from `~/.sam/config.toml`
- `SamClaudeClient` (sam-claude) — HTTP client with retry on 429/5xx
- `ConversationSession` (sam-claude) — per-handle history, token budget, tool-use loop
- `TokenBudget` (sam-claude) — daily token cap with midnight auto-reset
- `ChatDbReader` (sam-imessage) — read-only SQLite access to macOS chat.db

**Built-in tools (7):** memory_recall, memory_store, current_time, run_command, read_file, write_file, claude_code

## Configuration

Config file: `~/.sam/config.toml` with sections: identity, imessage, llm, memory, claude_code, safety.

API key loaded from `api_key_source` (supports `file:~/.sam/anthropic_key` or `env:VAR_NAME`).

System prompt loaded from `~/.sam/prompts/system.txt`.

## Safety

Destructive command patterns (rm -rf, sudo, git push --force, DROP TABLE, etc.) are blocked via pattern matching in `[safety]` config. The claude_code tool runs in `--print` (non-interactive) mode with a 2-hour timeout.

## Submodule

`vendor/memory-brain/` is a git submodule (github.com/yhc007/memory-brain). Clone with `--recurse-submodules`. It provides `memory-actor` for BGE-M3 embeddings with hash-based fallback.

## Language

The project documentation (PROJECT_SUMMARY.md) and some comments are in Korean. The codebase itself uses English identifiers.
