#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_dir"

require() {
  local pattern="$1"
  local file="$2"
  if ! rg -q --fixed-strings "$pattern" "$file"; then
    echo "missing admission invariant: '$pattern' in $file" >&2
    exit 1
  fi
}

reject() {
  local pattern="$1"
  local file="$2"
  if rg -q --fixed-strings "$pattern" "$file"; then
    echo "forbidden stale SPARQL behavior: '$pattern' in $file" >&2
    exit 1
  fi
}

routes="src-tauri/src/loopback_graph_job_routes.rs"
require "run_external_sparql_query" "$routes"
require "run_external_sparql_update" "$routes"
require "timeout_ms is not accepted for SPARQL updates" "$routes"
reject "StatusCode::ACCEPTED" "$routes"
reject "insert_finished(" "$routes"

require 'route("/api/sparql/query", post(loopback_graph_query_job))' \
  "src-tauri/src/loopback_rdf_routes.rs"
require 'route("/api/sparql/update", post(loopback_graph_update_job))' \
  "src-tauri/src/loopback_rdf_routes.rs"
require ".or(self.update.as_deref())" \
  "src-tauri/src/loopback_graph_inputs.rs"

admission="src-tauri/src/sparql_admission.rs"
require "spawn_blocking" "$admission"
require "with_cancellation_token" "src-tauri/src/rdf_query_service.rs"
require "cancellation.cancel();" "$admission"
require "GraphPersistenceCoordinator" "$admission"
require "declare_rdf_writes_self_tracked" "$admission"
require "require_no_active_restore" "$admission"
require "ensure_graph_store_seeded_with_cancellation" \
  "src-tauri/src/rdf_service.rs"
require "require_seed_not_cancelled" \
  "src-tauri/src/rdf_seed_service.rs"

registry="src-tauri/src/mcp_dispatch_registry.rs"
require "mcp_local_query_graph(app, args)" "$registry"
require "mcp_local_sparql_query(app, args)" "$registry"
require "mcp_local_sparql_update(app, args)" "$registry"
require "run_external_sparql_query" "src-tauri/src/mcp_graph_service.rs"
require "run_external_sparql_query" "src-tauri/src/rdf_service.rs"
require "run_external_sparql_update" "src-tauri/src/rdf_service.rs"
require "timeoutMs/timeout_ms is not accepted for SPARQL updates" \
  "src-tauri/src/rdf_service.rs"

restore="src-tauri/src/time_travel_restore_service.rs"
require "coordinator.acquire(graph_id).await" "$restore"
require "capture_restore_point_with_lease" "$restore"
require "let _restore_guard = restore_guard;" "$restore"
require "read_document_cold_with_lease" \
  "src-tauri/src/document_history_service.rs"
require "self_heal_missing_document_with_lease" \
  "src-tauri/src/document_service.rs"
require "read_document_with_lease" \
  "src-tauri/src/time_travel_service.rs"
require "declare_rdf_writes_self_tracked" \
  "src-tauri/src/time_travel_service.rs"

catalog="src-tauri/src/mcp_tool_catalog.json"
require '"timeoutMs"' "$catalog"
require '"maxRows"' "$catalog"
require '"timeout_ms"' "$catalog"
require '"max_rows"' "$catalog"

jq empty "$catalog"
require "'POST /graphs/query': 'SparqlQueryResult'" \
  "parity/openapi-route-contracts.mjs"
require "'POST /graphs/update': 'MutationResult'" \
  "parity/openapi-route-contracts.mjs"
require '"inline SPARQL query result"' \
  "parity/route-envelope-snapshot.json"
require '"properties": ["graphId", "maxRows", "query", "timeoutMs"]' \
  "parity/mcp-tool-schema-snapshot.json"
node parity/generate-openapi.mjs --check
jq empty \
  parity/local-openapi.json \
  parity/mcp-tool-schema-snapshot.json \
  parity/route-envelope-snapshot.json \
  parity/surface-classification.json \
  parity/surface-contracts.json
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check

echo "SPARQL admission static invariants: PASS"
