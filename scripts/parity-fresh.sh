#!/usr/bin/env bash
# Run parity:loopback against a freshly-wiped local profile.
#
# Each parity:loopback run leaves test debris in the local Tauri profile:
# graphs, journal entries, ledger entries, RDF state. After enough runs the
# accumulated state slows down or destabilizes the harness (we've seen 256+
# graphs make list_graphs take 12s and downstream ops time out at 60s).
#
# This script wipes the profile, restarts Tauri, waits for the loopback
# manifest to come up healthy, and runs the parity probe. It leaves Tauri
# running afterwards so subsequent iterations are fast.
#
# Usage: pnpm parity:loopback:fresh
set -euo pipefail

PROFILE_PARENT="$HOME/Library/Application Support/dev.sophia.garden/profiles"
PROFILE="$PROFILE_PARENT/default"
PROTOTYPE_DIR="$(cd "$(dirname "$0")/.." && pwd)"

echo "[parity-fresh] killing any running tauri..."
pkill -9 -f "garden" 2>/dev/null || true
pkill -9 -f "cargo-tauri tauri dev" 2>/dev/null || true
sleep 2

if [ -d "$PROFILE" ]; then
  TS=$(date +%Y-%m-%dT%H%M%S)
  BAK="${PROFILE}.bak-${TS}"
  echo "[parity-fresh] moving profile aside -> $(basename "$BAK")"
  mv "$PROFILE" "$BAK"
fi

echo "[parity-fresh] starting tauri dev (background)..."
TAURI_LOG="/tmp/parity-fresh-tauri-$$.log"
( cd "$PROTOTYPE_DIR" && pnpm tauri dev >"$TAURI_LOG" 2>&1 ) &
TAURI_WRAP_PID=$!

echo "[parity-fresh] waiting for tauri healthy (up to 240s)..."
deadline=$(($(date +%s) + 240))
ready=0
until [ $(date +%s) -ge $deadline ]; do
  if [ -f "$PROFILE/loopback.json" ]; then
    PORT=$(jq -r .port "$PROFILE/loopback.json" 2>/dev/null || echo "")
    PID=$(jq -r .pid "$PROFILE/loopback.json" 2>/dev/null || echo "")
    if [ -n "$PORT" ] && [ -n "$PID" ] && [ "$PID" != "null" ]; then
      if curl -s -m 1 -o /dev/null "http://127.0.0.1:$PORT/health" 2>/dev/null; then
        echo "[parity-fresh] ready: pid=$PID port=$PORT"
        ready=1
        break
      fi
    fi
  fi
  sleep 2
done

if [ "$ready" -ne 1 ]; then
  echo "[parity-fresh] tauri did not become healthy within 240s — see $TAURI_LOG" >&2
  kill -9 "$TAURI_WRAP_PID" 2>/dev/null || true
  exit 1
fi

echo "[parity-fresh] running parity:loopback..."
cd "$PROTOTYPE_DIR" && pnpm parity:loopback
RC=$?
echo "[parity-fresh] parity:loopback exited with code $RC"
exit "$RC"
