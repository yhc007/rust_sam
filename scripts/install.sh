#!/bin/bash
# Install Sam agent as a macOS LaunchAgent.
# Usage: ./scripts/install.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"
PLIST_SRC="$SCRIPT_DIR/com.sam.agent.plist"
PLIST_DST="$HOME/Library/LaunchAgents/com.sam.agent.plist"
BIN_SRC="$REPO_DIR/target/release/sam-agent"
BIN_DST="/usr/local/bin/sam-agent"
LOG_DIR="$HOME/.sam/logs"

echo "=== Sam Agent Installer ==="

# 1. Build release binary if needed
if [ ! -f "$BIN_SRC" ]; then
    echo "Building release binary..."
    (cd "$REPO_DIR" && cargo build -p sam-agent --release)
fi

# 2. Copy binary
echo "Installing binary → $BIN_DST"
sudo cp "$BIN_SRC" "$BIN_DST"
sudo chmod +x "$BIN_DST"

# 3. Create log directory
mkdir -p "$LOG_DIR"

# 4. Unload existing agent if running
if launchctl list | grep -q com.sam.agent; then
    echo "Stopping existing agent..."
    launchctl unload "$PLIST_DST" 2>/dev/null || true
fi

# 5. Install plist
echo "Installing LaunchAgent → $PLIST_DST"
cp "$PLIST_SRC" "$PLIST_DST"

# 6. Load agent
echo "Starting Sam agent..."
launchctl load "$PLIST_DST"

# 7. Verify
sleep 2
if launchctl list | grep -q com.sam.agent; then
    PID=$(launchctl list | grep com.sam.agent | awk '{print $1}')
    echo ""
    echo "✅ Sam agent running (PID: $PID)"
    echo "   Logs: $LOG_DIR/sam-agent.log"
    echo "   Errors: $LOG_DIR/sam-agent.err"
    echo ""
    echo "Commands:"
    echo "  Stop:    launchctl unload ~/Library/LaunchAgents/com.sam.agent.plist"
    echo "  Start:   launchctl load ~/Library/LaunchAgents/com.sam.agent.plist"
    echo "  Logs:    tail -f ~/.sam/logs/sam-agent.log"
    echo "  Status:  sam-agent status"
else
    echo "⚠️  Agent may not have started. Check: launchctl list | grep sam"
    echo "   Error log: tail ~/.sam/logs/sam-agent.err"
fi
