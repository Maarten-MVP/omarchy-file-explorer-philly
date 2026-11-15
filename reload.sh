#!/usr/bin/env bash
# Put the current source into use: build, install, and restart philly.
#
# Every philly window lives in one process, so this CLOSES ALL OPEN PHILLY
# WINDOWS (and any Open/Save dialog philly is showing for another app).
# Afterwards the background service runs the new build; open philly to check.
#
#   ./reload.sh
set -euo pipefail
cd "$(dirname "$0")"

BIN="$HOME/.local/bin/philly"

# Build and install first: if the build fails, the running philly is left alone.
./install.sh --no-takeover

if pgrep -x philly >/dev/null; then
  echo "→ Closing philly ($(pgrep -x philly | wc -l) process(es))…"
  pkill -x philly || true
  for _ in $(seq 50); do
    pgrep -x philly >/dev/null || break
    sleep 0.1
  done
  pkill -9 -x philly 2>/dev/null || true
fi

echo "→ Starting the background service…"
setsid -f "$BIN" --service >/dev/null 2>&1
for _ in $(seq 30); do
  pgrep -x philly >/dev/null && break
  sleep 0.1
done
if ! pgrep -x philly >/dev/null; then
  echo "philly did not start. Try: $BIN --service" >&2
  exit 1
fi
echo "Done. philly runs the new build: open it (Super+Shift+F) to check."
