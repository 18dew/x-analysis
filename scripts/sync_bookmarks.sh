#!/usr/bin/env bash
# Keep the local X bookmarks archive fresh — designed for cron/launchd.
#
# Example cron entry (syncs every 30 minutes, logging to ~/.xanalysis/data):
#   */30 * * * * "$HOME/Codebase/git/Xanalysis/scripts/sync_bookmarks.sh"
#
# Environment overrides:
#   XANALYSIS_BIN   path to the xanalysis binary (default: ~/.cargo/bin/xanalysis)
#   XANALYSIS_HOME  data home (default: ~/.xanalysis)
#   XANALYSIS_LOG   log file (default: $XANALYSIS_HOME/data/sync.log)

set -euo pipefail

BIN="${XANALYSIS_BIN:-$HOME/.cargo/bin/xanalysis}"
HOME_DIR="${XANALYSIS_HOME:-$HOME/.xanalysis}"
LOG="${XANALYSIS_LOG:-$HOME_DIR/data/sync.log}"

mkdir -p "$(dirname "$LOG")"

{
  echo "[$(date -Iseconds)] starting sync"
  if ! "$BIN" sync; then
    echo "[$(date -Iseconds)] sync FAILED"
    exit 1
  fi
  echo "[$(date -Iseconds)] sync OK"
} >> "$LOG" 2>&1
