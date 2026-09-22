#!/usr/bin/env bash
# Build the lightweight HEADLESS gardend cell binary.
#
# gardend is the GUI-free garden core: it boots the loopback REST/MCP server,
# storage, CRDT queue, and scheduler on Garden's native headless facade — no
# Tauri, webview, or desktop plugins. It is BOTH:
#   * the per-graph cell binary for platform-next, AND
#   * the batteries-included local backend that `sophia-mcp` spawns (sophia-mcp
#     discovers it on PATH or via SOPHIA_MCP_GARDEN_BIN).
#
# Usage:
#   ./build-gardend-headless.sh            # release build
#   ./build-gardend-headless.sh --check    # cargo check only (fast, no codegen)
#
# Output (release): target/release/examples/gardend
#
# gardend is an [[example]] (not a [[bin]]) so the desktop Tauri bundler — which
# copies every manifest [[bin]] into the .app — ignores it. It is still a real,
# standalone `gardend` binary; it just lands under examples/.
set -euo pipefail

cd "$(dirname "$0")"

FEATURES=(--no-default-features --features headless --example gardend)

if [[ "${1:-}" == "--check" ]]; then
  echo "==> cargo check (headless gardend)"
  exec cargo check "${FEATURES[@]}"
fi

echo "==> cargo build --release (headless gardend)"
cargo build --release "${FEATURES[@]}"
echo "==> built: $(pwd)/target/release/examples/gardend"
