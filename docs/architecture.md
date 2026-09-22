# Local Native Architecture

This document is the implementation map for the local-native Tauri app. It
describes the shape the code should keep converging toward while the prototype
is still allowed to move quickly.

## Core Rule

CRDT state is authoritative.

The local app may persist JSON manifests, Oxigraph triples, flat block
projections, search records, and job rows, but those are projections or caches.
Document and workspace mutations should flow through Yjs-backed operations, then
materialize outward into the other stores.

## Runtime Shape

The local app has four layers:

1. Frontend runtime adapters in `frontend/src/native/`.
2. Tauri commands in `src-tauri/src/lib.rs` and focused service modules.
3. Loopback REST and MCP adapters under `src-tauri/src/loopback_*` and
   `src-tauri/src/mcp_*`.
4. Local provider services for storage, CRDT operations, RDF, search,
   ingestion, jobs, and semantic models.

Tauri, REST, and MCP are adapters. They should call shared service modules and
avoid owning storage paths, RDF delete policy, CRDT serialization, or job state
transitions directly.

## Service And Error Boundaries

Service modules should expose typed records and `AppResult<T>` at their internal
boundary. Adapter layers may still collapse errors to strings where existing
Tauri, MCP, or local job contracts require that shape, but the service should
classify failures before that edge. Direct loopback responses should use typed
`AppErrorKind` mapping instead of substring tests such as `"not found"`.

The graph catalog path is the first cleaned boundary:

- `graph_record_store` returns `AppResult` and classifies missing graphs as
  `NotFound`.
- `graph_service` exposes typed graph service functions for list/create/update
  and soft delete, with Tauri command wrappers converting only at the command
  edge.
- Direct graph loopback handlers use `loopback_app_result`; hosted-shaped job
  submissions keep string job-result errors until the job subsystem receives the
  same typed boundary.

## Authority Boundaries

- `profile.json` and `identity.json` describe the local profile.
- Profile Turso graph rows in `metadata.turso` describe local graph catalog
  metadata; JSON graph records remain sidecars for inspection, cache bootstrap,
  and archive/export flows.
- The profile metadata graph mirrors the graph catalog into Oxigraph.
- Workspace Yjs state owns folders, ordering, artifacts, wires, and graph UI
  state.
- Document Yjs state owns editable document content.
- Document JSON/XML/tree/block files are inspection and read-model projections.
- Graph Oxigraph stores own RDF projection and user-imported RDF datasets.
- Semantic indexes are replaceable local caches over block projections.
- Turso job rows are local cache/control records for long-running work.
- Durable worklog records are the acceptance boundary for local work that may
  outlive a request, including CRDT writes and long-running jobs.
- Original files are durable blobs referenced by document, artifact, or image
  manifests.

Because no released local profile exists, this tree is greenfield local state.
The current work should optimize for clear boundaries and correct behavior
rather than legacy profile migration.

## Named Graph Policy

RDF materializers must keep ownership explicit:

- The profile metadata graph describes the profile's graph catalog:
  `urn:mnemosyne:local:profile:{profile_id}:meta`.
- Graph record projection triples live under
  `urn:mnemosyne:local:graph:{graph_id}:projection:graph`.
- Workspace materialization owns folders, artifacts, document metadata, wires,
  and section tree metadata under
  `urn:mnemosyne:local:graph:{graph_id}:projection:workspace`.
- Document materialization owns document content subjects under the document
  namespace, stored in
  `urn:mnemosyne:local:graph:{graph_id}:projection:document:{document_id}`.
- User RDF imports default to
  `urn:mnemosyne:local:graph:{graph_id}:user:rdf` or a caller-selected
  non-reserved named graph.

Projection materializers must only delete/insert inside their own projection
named graph. User RDF load/update paths must reject reserved projection graph
targets. This policy prevents projection refreshes from deleting user-authored
RDF and prevents direct local RDF writes from mutating projection authority.

## Module Direction

The old monolith has been split into focused modules. The current direction is:

- `storage*`, `*_paths`, `pending_upload_paths`, and `pending_upload_service`
  own file discipline and graph/profile pending-file handoff.
  `paths` remains a compatibility facade over focused profile, graph,
  document/original-file, Y.Doc, and semantic index path modules.
- `graph_catalog_store` and `profile_metadata_db` own the profile-local Turso
  graph catalog cache, while `graph_service` owns catalog mutations.
- `graph_usage_service` owns local graph storage-size and hosted-shaped graph
  stats projections.
- `local_job*` separates Turso connection/schema setup, job-record
  persistence, registry state transitions, and result spill files.
- `crdt_*`, `document_*`, and `workspace_*` own CRDT operation, audit, and
  projection behavior. `crdt_operation_journal` owns the durable write-intent
  log used to recover queued local writes after restart.
- `loopback_client_token_*` owns scoped token types, scope resolution,
  hash/expiry behavior, secret persistence, lifecycle/audit behavior, and
  bearer-token resolution.
- `original_file_*` separates source-blob read/write, manifest repair, access
  tokens, and HTTP response shaping for local document/artifact/image files.
- `rdf_*` owns Oxigraph access and materialization.
- `salience_*` separates route input contracts, value-store/config behavior,
  RDF materialization, and score/config projections.
- `semantic_model_*` separates local model catalog specs, runtime/cache
  inspection, setup/config state, and status response contracts.
- `semantic_*` owns embedding lifecycle, indexing, and search.
- `loopback_*` and `mcp_*` expose adapters over the provider services.

Further crate splitting is optional until module boundaries stop carrying their
weight. The important requirement is that new behavior lands in the owning
service layer first, then gets exposed through Tauri, REST, and MCP.

## Contributor Module Map

Use this map before adding new files or routes.

### Provider Boundary

Shared local-provider behavior should live in service modules, not directly in
adapter handlers. The main adapter families are:

- Tauri commands: `lib.rs` command registrations plus focused command/service
  modules.
- Loopback REST: `loopback_router.rs` delegates to `loopback_*_routes.rs`.
- MCP: `mcp_tool_registry.rs`, `mcp_tool_dispatch.rs`, and focused
  `mcp_*_service.rs` modules.

When adding behavior, implement the service first, then expose it through the
required adapters.

### CRDT Authority

- `crdt_queue.rs`, `crdt_operation_queue.rs`, `crdt_operation_types.rs`, and
  `crdt_operation_audit.rs` own queued operation contracts, timing, and audit.
- `crdt_operation_journal.rs` records queued/completed operation events before
  the queue is treated as accepted work; local-mode startup re-enqueues
  non-terminal operations for the TypeScript CRDT runtime to drain.
- `crdt_projection_flush.rs` owns the explicit read-after-write boundary for
  adapters that promise projection-backed responses. Use it instead of ad hoc
  `crdt.flush` calls when a REST/MCP route writes CRDT state and then returns
  navigation, hosted document/entity, RDF, search, or job-result data.
- `document_mutation_service.rs`, `document_mcp_service.rs`,
  `document_mcp_blocks.rs`, and `workspace_*` modules own document/workspace
  mutations and projections.
- `active_documents.rs` only tracks warm live projections; it must degrade to
  cold disk reads if unavailable.

Do not let REST or MCP routes mutate document/workspace state by writing
projection files directly.

Raw CRDT routes and block-edit hot paths may return at the durable/live
authority boundary. Hosted-compatible routes and MCP tools that return
projection-backed data must either read directly from live authority or force a
targeted projection flush before returning.

### Loopback Security

- `loopback_http.rs` owns bearer extraction, origin checks, scope checks, and
  status mapping helpers.
- `loopback_scopes.rs`, `loopback_scope_catalog.rs`,
  `loopback_token_grants.rs`, and `loopback_client_token_*` own scope names,
  descriptors, grant profiles, named token storage, lifecycle, and audit.
- `loopback_audit_log.rs` owns redacted local JSONL audit persistence.
- `docs/loopback-security.md` describes the implementation contract.
- `docs/loopback-threat-model.md` describes the public-alpha threat model.

Adding a mutating route or MCP tool means adding the scope, registry metadata,
handler enforcement, negative authorization coverage, and audit posture.

### RDF Ownership

- `rdf_authority.rs` rejects direct user writes into reserved projection/profile
  named graphs.
- `rdf_record_materializer.rs`, `rdf_workspace_materializer.rs`,
  `rdf_wire_materializer.rs`, `rdf_document_tree.rs`, and
  `graph_metadata_materializer.rs` own projection writes.
- `rdf_query_service.rs`, `rdf_import_service.rs`, and `rdf_service.rs` expose
  user/query/import behavior.

Projection materializers must delete/insert only inside their own named graph.
User RDF paths must default to user RDF graphs and reject reserved projection
targets.

### Jobs

- `local_job_db.rs`, `local_job_store.rs`, `local_job_registry.rs`,
  `local_job_transitions.rs`, `local_job_record_builder.rs`, and
  `local_job_results.rs` own persisted local job records and result spill files.
- Route/MCP job wrappers should return hosted-shaped job records while keeping
  execution local.
- MCP tools that submit large work should return job envelopes and use
  `get_job_status`, `get_job_result`, and `cancel_job` instead of blocking the
  JSON-RPC call until parsing or CRDT writes finish.
- Long-running imports, PDF runtimes, semantic indexing, model setup, and
  future restore work should use jobs instead of synchronous request paths.
- `docs/local-worklog-architecture.md` describes the shared acceptance model for
  durable CRDT writes and async large-work. New heavy work should create a
  durable record before returning `202 Accepted` or a hosted-shaped job envelope.
- Async CRDT-backed jobs should not mark a job finished until any promised
  projection target has been flushed, otherwise a completed job can still race a
  follow-up hosted read.

### Ingestion And Files

- `pending_upload_paths.rs`, `multipart_pending_upload.rs`, and
  `pending_upload_service.rs` own pending-file creation, streaming writes,
  path validation, and cleanup.
- `original_file_*` modules own durable source-file adoption, manifests, access
  tokens, and HTTP download responses.
- `artifact_upload_service.rs`, `loopback_artifact_routes.rs`,
  `pdf_ingest_*`, `loopback_import_*`, and `rdf_import_service.rs` own
  specific import/upload surfaces.

Fresh upload paths should stream or copy into `pending-uploads/`, enqueue a
`pendingOriginalPath`, and let the runtime adopt the file. Avoid queued
`dataBase64` except for explicitly documented compatibility paths.

### Semantic And Salience

- `semantic_model_*` owns local model catalog, runtime setup, status, and
  selection state.
- `semantic_index*`, `semantic_search*`, and `semantic_embedder*` own local
  embedding/index/search behavior.
- `salience_*` owns value stores, scoring, route inputs, MCP valuation, and RDF
  materialization.

Semantic refresh and model work should expose progress/cancellation through the
local job system when they can run longer than an ordinary request.

### Parity And Gates

- `parity/local-loopback-surface.json` is the local route/tool registry.
- `parity/generate-openapi.mjs` produces the local OpenAPI snapshot.
- `parity/check-surface-inventory.mjs` validates route/MCP/scope coverage.
- `parity/check-tauri-security.mjs` validates shell/security baseline.
- `parity/check-storage-discipline.mjs` validates storage and pending-file
  discipline.

Any public surface change should update the registry/snapshots and keep these
checks green.

## Common Change Recipes

### Add A Loopback Route

1. Add or reuse the local-provider service function.
2. Add the route handler in the owning `loopback_*_routes.rs` module.
3. Enforce scopes with `require_loopback_scope` or `require_loopback_scopes`.
4. Register the path in `loopback_router.rs` through the owning router.
5. Update `parity/local-loopback-surface.json`.
6. Regenerate/check OpenAPI and inventory.

### Add An MCP Tool

1. Add schema and metadata to `mcp_tool_catalog.json`.
2. Add scope metadata in `local-loopback-surface.json`.
3. Add dispatch in `mcp_tool_dispatch.rs`.
4. Put behavior in a focused `mcp_*_service.rs` module.
5. Keep `tools/list` metadata and inventory checks green.

### Add An Ingestion Approach

1. Add the engine descriptor in `ingestion_approach_static.rs` or the PDF
   pipeline catalog modules.
2. Add runtime/status detection if the engine depends on local binaries,
   Python, OCR, or model files.
3. Use pending-file handoff for source bytes.
4. Use local jobs for long-running parse/OCR/model work.
5. Return hosted-shaped import/job responses.

### Add A Scope

1. Add the scope to `loopback_scopes.rs` / scope catalog with category, access,
   description, and default-grant posture.
2. Include it in grant profiles only when appropriate.
3. Enforce it in REST and MCP handlers.
4. Update route/tool registry metadata.
5. Add or update negative authorization coverage.

## Performance Direction

Local mode is allowed to diverge from hosted internals when that makes the
desktop path better:

- Use in-process or local-file work instead of Redis/S3/DynamoDB facsimiles.
- Use graph-local Oxigraph store caches instead of reopening stores per request.
- Stream multipart uploads into pending files before parsers or CRDT shims read
  them.
- Put profile graph catalog and job metadata in Turso; spill large job result
  JSON to files.
- Keep heavy PDF, OCR, embedding, and import work behind pollable jobs.

The target is hosted-shaped contracts with local-native execution.
