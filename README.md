# Sam

Personal AI agent that communicates via iMessage, powered by Claude API.

## Architecture

```
┌─────────────┐     ┌──────────────┐     ┌─────────────┐
│  iMessage    │────▶│  sam-agent    │────▶│  Claude API  │
│  (chat.db)   │◀────│  (daemon)    │◀────│  (tool_use)  │
└─────────────┘     └──────────────┘     └─────────────┘
                          │
                    ┌─────┴─────┐
                    │  memory   │
                    │  brain    │
                    └───────────┘
```

## Crates

| Crate | Description |
|-------|-------------|
| `sam-core` | Shared types, config, paths |
| `sam-imessage` | iMessage poller & sender |
| `sam-claude` | Claude API client with tool_use |
| `sam-tools` | External tool registry |
| `sam-memory-adapter` | Adapter for memory-brain |

## Setup

```bash
# Clone with submodules
git clone --recurse-submodules https://github.com/yhc007/rust_sam.git

# Build
cargo build -p sam-agent

# Run daemon
SAM_LOG=info cargo run -p sam-agent -- daemon
```

## Configuration

Config file: `~/.sam/config.toml`

## License

MIT
