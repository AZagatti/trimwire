#!/usr/bin/env bash
# trimwire Tier 3 minimal POC — tmux send-keys restart (fallback mechanism).
#
# Last-resort path for the case where the gateway is unavailable. Assumes
# the user runs `claude` inside a tmux pane. Sends Ctrl+C to interrupt the
# current Claude prompt, then `claude --resume <id>` Enter to restart in the
# same pane, so the post-sweep JSONL is re-read.
#
# Usage:
#   trimwire-restart --session-id <UUID> [--target-pane <session:window.pane>]
#                   [--dry-run] [--timeout 5]
set -euo pipefail

SESSION_ID=""
TARGET_PANE=""
DRY_RUN=0
TIMEOUT=5

die() { echo "[restart] ERROR: $*" >&2; exit 1; }
log() { echo "[restart] $*" >&2; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --session-id)  SESSION_ID="$2"; shift 2 ;;
    --target-pane) TARGET_PANE="$2"; shift 2 ;;
    --dry-run)     DRY_RUN=1; shift ;;
    --timeout)     TIMEOUT="$2"; shift 2 ;;
    -h|--help)
      sed -n '2,/^set -e/p' "$0" | head -n -1; exit 0 ;;
    *) die "unknown arg: $1" ;;
  esac
done

[[ -z "$SESSION_ID" ]] && die "missing --session-id"
command -v tmux >/dev/null 2>&1 || die "tmux not on PATH"

# Detect tmux (this is a hard requirement of Tier 3)
if [[ -z "${TMUX:-}" ]]; then
  die "not inside a tmux session (\$TMUX is empty). Tier 3 requires tmux."
fi

# Default target pane = caller's own pane
if [[ -z "$TARGET_PANE" ]]; then
  TARGET_PANE="$(tmux display-message -p '#S:#I.#P')"
fi

# Verify target pane exists
if ! tmux list-panes -a -F '#S:#I.#P' | grep -qx "$TARGET_PANE"; then
  die "target pane '$TARGET_PANE' does not exist"
fi

CMD_BEFORE="$(tmux display-message -t "$TARGET_PANE" -p '#{pane_current_command}')"
log "target=$TARGET_PANE current_cmd=$CMD_BEFORE session_id=$SESSION_ID dry_run=$DRY_RUN"

if [[ "$DRY_RUN" -eq 1 ]]; then
  log "DRY-RUN: would send C-c, then 'claude --resume $SESSION_ID' Enter"
  exit 0
fi

# Step 1: cancel any current prompt
tmux send-keys -t "$TARGET_PANE" C-c
sleep 0.3

# Step 2: send the resume command
tmux send-keys -t "$TARGET_PANE" "claude --resume $SESSION_ID" Enter

# Step 3: poll for the pane's foreground command to become "claude"
log "polling up to ${TIMEOUT}s for 'claude' to appear..."
start_ts=$(date +%s)
while (( $(date +%s) - start_ts < TIMEOUT )); do
  cmd_now="$(tmux display-message -t "$TARGET_PANE" -p '#{pane_current_command}')"
  if [[ "$cmd_now" == "claude" || "$cmd_now" == *"claude"* ]]; then
    log "SUCCESS: claude running in $TARGET_PANE (cmd=$cmd_now)"
    exit 0
  fi
  sleep 0.2
done

CMD_AFTER="$(tmux display-message -t "$TARGET_PANE" -p '#{pane_current_command}')"
log "TIMEOUT: claude not detected after ${TIMEOUT}s (current cmd=$CMD_AFTER)"
exit 2
