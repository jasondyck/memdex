#!/bin/bash
# Nightly: shared agent vault (~/memory, with git history) + every Claude Code memory note
# → $MEMDEX_BACKUP_DIR (default ~/backups/memdex), 14 days kept.
set -euo pipefail
dest="${MEMDEX_BACKUP_DIR:-$HOME/backups/memdex}"; mkdir -p "$dest"
out="$dest/memory-$(date +%F).tar.gz"
cd "$HOME"
tar -czf "$out.tmp" memory .claude/projects/*/memory
mv "$out.tmp" "$out"
find "$dest" -name 'memory-*.tar.gz' -mtime +14 -delete
