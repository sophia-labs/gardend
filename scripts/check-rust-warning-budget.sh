#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST_PATH="${GARDEN_RUST_MANIFEST_PATH:-$ROOT_DIR/src-tauri/Cargo.toml}"
CARGO_BIN="${CARGO:-cargo}"

# Baseline after the CRDT durability follow-up: 274 dead_code + 7 unused_imports + 3 unused_mut.
# Reductions pass and should ratchet this number downward; increases fail.
DEFAULT_WARNING_BUDGET=284
WARNING_BUDGET="${GARDEN_RUST_WARNING_BUDGET:-$DEFAULT_WARNING_BUDGET}"

if ! [[ "$WARNING_BUDGET" =~ ^[0-9]+$ ]]; then
  echo "rust warning budget must be a non-negative integer, got: $WARNING_BUDGET" >&2
  exit 2
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required to evaluate Cargo's JSON diagnostics" >&2
  exit 2
fi

REPORT="$(mktemp "${TMPDIR:-/tmp}/garden-rust-check.XXXXXX")"
trap 'rm -f "$REPORT"' EXIT

set +e
"$CARGO_BIN" check \
  --manifest-path "$MANIFEST_PATH" \
  --message-format=json \
  "$@" >"$REPORT"
CARGO_STATUS=$?
set -e

WARNING_COUNT="$(
  jq -s '[.[] | select(.reason == "compiler-message" and .message.level == "warning")] | length' \
    "$REPORT"
)"
WARNING_SUMMARY="$(
  jq -r -s '
    [.[]
      | select(.reason == "compiler-message" and .message.level == "warning")
      | (.message.code.code // "uncoded")]
    | group_by(.)
    | map("\(.[0])=\(length)")
    | join(", ")
  ' "$REPORT"
)"

printf 'Rust warning budget: %s/%s' "$WARNING_COUNT" "$WARNING_BUDGET"
if [[ -n "$WARNING_SUMMARY" ]]; then
  printf ' (%s)' "$WARNING_SUMMARY"
fi
printf '\n'

if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  {
    printf '### Rust warning budget\n\n'
    printf '| Current | Budget | Breakdown |\n'
    printf '| ---: | ---: | --- |\n'
    printf '| %s | %s | %s |\n' "$WARNING_COUNT" "$WARNING_BUDGET" "${WARNING_SUMMARY:-none}"
  } >>"$GITHUB_STEP_SUMMARY"
fi

if (( CARGO_STATUS != 0 )); then
  jq -r '
    select(.reason == "compiler-message" and .message.level == "error")
    | .message.rendered // empty
  ' "$REPORT" >&2
  echo "cargo check failed with exit code $CARGO_STATUS" >&2
  exit "$CARGO_STATUS"
fi

if (( WARNING_COUNT > WARNING_BUDGET )); then
  jq -r '
    select(.reason == "compiler-message" and .message.level == "warning")
    | .message.rendered // empty
  ' "$REPORT" >&2
  echo "::error title=Rust warning budget exceeded::$WARNING_COUNT warnings exceed the budget of $WARNING_BUDGET" >&2
  exit 1
fi

if (( WARNING_COUNT < WARNING_BUDGET )); then
  echo "::notice title=Rust warning budget reduced::Lower DEFAULT_WARNING_BUDGET from $WARNING_BUDGET to $WARNING_COUNT" >&2
fi
