#!/bin/bash
# Rotate Sam agent logs. Keep last 7 days.
# Add to crontab: 0 5 * * * ~/.sam/scripts/log-rotate.sh

LOG_DIR="$HOME/.sam/logs"
KEEP_DAYS=7

for f in "$LOG_DIR"/sam-agent.log "$LOG_DIR"/sam-agent.err; do
    if [ -f "$f" ] && [ "$(wc -c < "$f")" -gt 0 ]; then
        DATE=$(date +%Y%m%d)
        mv "$f" "${f}.${DATE}"
        touch "$f"
    fi
done

# Remove old rotated logs
find "$LOG_DIR" -name "*.log.*" -mtime +$KEEP_DAYS -delete 2>/dev/null
find "$LOG_DIR" -name "*.err.*" -mtime +$KEEP_DAYS -delete 2>/dev/null
