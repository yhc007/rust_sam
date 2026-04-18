#!/bin/bash
# Uninstall Sam agent LaunchAgent.
# Usage: ./scripts/uninstall.sh

set -euo pipefail

PLIST="$HOME/Library/LaunchAgents/com.sam.agent.plist"

echo "=== Sam Agent Uninstaller ==="

if [ -f "$PLIST" ]; then
    echo "Stopping agent..."
    launchctl unload "$PLIST" 2>/dev/null || true
    rm "$PLIST"
    echo "LaunchAgent removed."
else
    echo "LaunchAgent not found."
fi

if [ -f /usr/local/bin/sam-agent ]; then
    echo "Removing binary..."
    sudo rm /usr/local/bin/sam-agent
    echo "Binary removed."
fi

echo "✅ Done. Logs preserved at ~/.sam/logs/"
