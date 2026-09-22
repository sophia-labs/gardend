# API and MCP Surface Parity - Analysis and Roadmap

Status date: 2026-05-01

This document summarizes where the local native Tauri POC stands on overall API
and MCP parity, then sequences the next work by leverage. It is grounded in
this `garden` repo, the broader local-native reports under `docs/reports/`
(in `mnemosyne-platform`), and the Open-Core Native workspace notes in
Mnemosyne.

## Sources Reviewed

- `README.md`
- `parity/local-loopback-surface.json`
- `parity/surface-classification.json`
- `parity/check-surface-inventory.mjs`
- `parity/check-loopback-surface.mjs`
- `parity/compare-stacks.mjs`
- `docs/reports/local-native-api-mcp-parity.md`
- `docs/reports/local-native-original-local-comparison.md`
- `docs/reports/local-native-tauri-poc.md`
- Mnemosyne workspace folder: `Work / Strategy and Planning / Open-Core Native`

## Current State

The local-native stack is now on the right architectural track. It is not a
cloud clone. The local loopback API, MCP endpoint, Tauri commands, and frontend
runtime are converging on thin adapters over the same provider core:

- Workspace and document CRDT state remain authoritative.
- Manifests, RDF, OpenAPI, search indexes, flat blocks, and source-file records
  are derived projections.
- Hosted-shaped HTTP and MCP surfaces are adapters over local files, Yjs, and
  Oxigraph, not parallel business logic.
- Cloud mechanics such as Redis, S3, DynamoDB, Cognito, worker pods, and hosted
  billing stay outside the local-only critical path.

The static inventory is green and gives the current shape:

| Surface | Current signal |
| --- | --- |
| Hosted routes scanned | 180 total, including 5 WebSockets |
| Hosted route classifications | 41 implemented, 10 adapter-ready, 41 core-missing, 47 local-replacement, 41 hosted-only |
| Local loopback routes | 75 declared and present |
| Local OpenAPI | 63 paths, 75 operations |
| Hosted MCP tools scanned | 55 total |
| Hosted MCP classifications | 18 implemented, 6 adapter-ready, 27 core-missing, 4 local-replacement |
| Local MCP tools | 28 implemented, plus 9 local extensions |

The live loopback harness now covers meaningful product behavior, not only
health checks: materialization, read adapters, hosted aliases, MCP writes,
block mutations, wire mutations, artifact metadata, Markdown/HTML/EPUB/PDF
uploads, batch upload, image upload, and local CRDT timing traces.

The most recent rebuilt isolated loopback proof showed:

- Markdown upload: 85.2 ms.
- HTML upload: 87.4 ms.
- EPUB upload: 203.4 ms.
- PDF upload: 2056.5 ms.
- Batch upload: 74.9 ms.
- MCP write: 76.6 ms.
- Hosted-shaped write: 67.6 ms.

That points to a clear performance boundary: common CRDT-backed writes are now
acceptable, while PDF and heavier ingestion remain job/offload work.

## Important Caveats

The inventory being green is not the same as product parity being complete.
It proves that discovered surfaces are classified and local declared routes are
covered by OpenAPI. It does not prove exact response-shape parity, value-level
semantics, scope policy, or long-running job behavior.

`parity/surface-contracts.json` now separates the axes explicitly. `accepted`
means schema and semantics are accepted for the current local contract.
`deferred` means the surface may be present and enabled, but its remaining
behavior gaps are named in the contract note or untested reason. The inventory
gate enforces that accepted entries have accepted schema/semantics and that
deferred entries carry a reason.

There is also minor classification drift. One concrete example: the local MCP
surface now exposes `write_document`, but `surface-classification.json` still
classifies the hosted MCP `write_document` tool as `core-missing`. That should
be corrected only after its semantics are accepted as matching the hosted tool
closely enough, but the discrepancy shows why a contract-truth pass should come
before adding many more endpoints.

## Parity Definition

Parity means external product compatibility, not implementation identity.

Must match exactly:

- Hosted route paths and response envelopes for surfaces existing clients call.
- MCP tool names, argument names, result semantics, and durable side effects.
- Document CRDT field names, especially `Y.XmlFragment("content")`.
- Workspace CRDT maps for folders, documents, artifacts, wires, and UI state.
- Block IDs, TipTap node/mark attrs, comments, list normalization, image/math
  attrs, wire endpoints, and source-file `sf_*` fields.
- Profile graph-catalog metadata is a derived metadata graph, not content graph
  authority.
- RDF ownership split: workspace materializes folder/artifact/document metadata
  and wires; document materialization owns content subjects.
- Explicit unsupported behavior through capability metadata or stable
  `501 not_implemented` errors.

Should diverge locally:

- Redis queues become profile-local Turso job metadata plus file-spilled large
  results.
- S3/DynamoDB become local profile files and manifests.
- Hocuspocus is not required for in-process Tauri editing.
- Hosted auth, billing, sharing, public aliases, waitlist, cloud diagnostics,
  and production metrics are hosted-only or local-replacement surfaces.

## Roadmap By Leverage

### Phase 0 - Contract Truth and Gates

Goal: make the inventory a product-trustworthy map before expanding the surface.

High-leverage work:

- Reconcile stale classifications, starting with `write_document`.
- Split "route exists" from "semantics accepted" in the surface inventory.
- Add response-envelope snapshots for implemented hosted aliases.
- Add MCP schema snapshots for implemented and adapter-ready tools.
- Keep `parity:openapi`, `parity:inventory`, and `parity:loopback` as the base
  gate, but add targeted assertions for exact keys and error envelopes.
- Record performance budgets for common write, read, search, upload, and
  navigation paths.

Acceptance gate:

- Every implemented route/tool has a classification, an owner adapter, a schema
  assertion, and at least one live parity probe or an explicit reason it cannot
  be live-tested yet.

### Phase 1 - Finish Thin API Adapters Over Existing Primitives

Goal: close high-visibility HTTP gaps where the local provider already has most
of the underlying data.

High-leverage work:

- Graph metadata: detail, update, delete, duplicate, export/import shell,
  stats/properties/summary where local equivalents are already derivable.
- Document routes: flush, duplicate, export, blob/workspace blob, description,
  and any remaining read aliases over existing projections.
- Navigation routes: hosted-shaped folder create/update/move behavior, stronger
  non-empty folder behavior, and artifact/folder edge cases.
- Search routes: reindex and rematerialize job aliases over local indexing and
  materialization.
- Wire routes: harden create/delete/refresh/traverse semantics and tombstones
  now that basic wire mutations exist.
- OpenAPI: replace generic schemas on implemented hosted aliases with precise
  hosted-compatible schemas.

Acceptance gate:

- `parity:compare --include-mutations --strict` has no shape drift across the
  shared route set for seeded fixture graphs.

### Phase 2 - Promote MCP To A First-Class Adapter

Goal: make MCP use the same provider, CRDT, auth, scope, and schema machinery as
HTTP instead of remaining a hand-maintained side channel.

High-leverage work:

- Local scopes: loopback manifests and MCP tool lists now advertise explicit
  read/search/write/delete/artifact/RDF/MCP scope metadata. Next step is
  issuing per-client scoped tokens instead of granting the full `session-all`
  scope set to every manifest bearer token.
- Orientation/session replacements: `get_user_location`, `get_session_state`,
  `set_home_graph`, then `quick_orient` and `context_bundle`.
- Graph/job adapters: `create_graph`, `query_graph`, `update_graph`,
  `cancel_job`, and `duplicate_graph` where local provider support exists.
- Workspace tools: `move_folder`, `rename`, and generic multi-target `delete`.
- Wires: `create_wires`, `traverse_wires`, delete/refresh through the workspace
  Y.Doc.
- Artifact shell: `upload_artifact` and `ingest_artifact` once the job/offload
  path exists.

Acceptance gate:

- Hosted and local MCP tool schemas can be diffed mechanically, and every
  enabled mutating tool routes through the CRDT/provider path rather than direct
  manifest/RDF edits.

### Phase 3 - Deep CRDT Document Fidelity

Goal: match hosted/MCP document semantics, not just whole-document round trips.

High-leverage work:

- Accept `write_document` semantics as implemented only after fixtures cover
  Markdown, TipTap XML, HTML, comments, list items, marks, and block IDs.
- Extend block operations for full hosted attr coverage: indent, list type,
  checked state, code attrs, headings, links, inline marks, images, math, and
  tables.
- Add `edit_comment`, `insert_calendar_event`, and `make_document_editable`.
- Add live projection coverage for workspace-scoped reads where useful.
- Port durable fixtures from `../mnemosyne-mcp/tests` into the local parity
  harness.

Acceptance gate:

- Block/document mutation fixtures pass against both local and hosted MCP where
  hosted behavior is stable; known hosted defects are recorded as expected
  legacy issues rather than hidden as local parity failures.

### Phase 4 - Artifact, Import, and Job Offload

Goal: prevent large ingestion from blocking Tauri/WebKit while filling the
artifact/import API family.

High-leverage work:

- Treat loopback uploads and imports as jobs with progress, cancellation, and
  result endpoints.
- Move PDF parsing, OCR/accurate extraction, Y.Doc snapshot generation, and RDF
  materialization out of paths that can block the UI.
- Replace multipart/base64 duplication with temp-file or streaming handoff
  wherever the parser can consume local files. PDF accurate ingestion, generic
  artifact upload, image upload, and RDF import now stream multipart file fields
  into graph-local pending files first; remaining base64 payloads are
  compatibility shims for CRDT/frontend parsers that still require inline
  bytes.
- Add DOCX, better EPUB, searchable-PDF, OCR-PDF, web clip, RDF import, and
  graph bundle import/export behind stable job envelopes.
- Keep original bytes durable and source metadata hosted-shaped.

Acceptance gate:

- Large real-sample ingestion remains responsive, cancellable, and recoverable
  after restart, with no placeholder documents left as successful imports.

### Phase 5 - Salience, Values, Memory, and History

Goal: close the semantic-agent parts of MCP that need local durable models
rather than adapter wrappers.

High-leverage work:

- Local valuation store and RDF/search materialization for block values.
- MCP `value`, `get_values`, `get_block_values`, `revaluate`, and
  `get_important_blocks`.
- Memory queue tools: `remember`, `recall`, `archive_memories`, and `care`.
- Narrative tools: `music`, `sing`, `surface`, and `dump_chat`, with local
  workspace semantics instead of cloud assumptions.
- Append-only document history, `get_document_history`, snapshots, and
  `read_document_at_snapshot`.

Acceptance gate:

- Values, memory, and history survive restart and can be queried through both
  MCP and local API/RDF surfaces without sidecar-only state.

### Phase 6 - Hybrid Runtime, Mirror Sync, and Local Replacements

Goal: expose local, hosted, and mirrored graphs through a coherent provider
model without weakening auth/privacy boundaries.

High-leverage work:

- Finish per-graph runtime dispatch so `nativeLocal` is no longer a process-wide
  product fork.
- Add hosted login/token storage in Tauri only after local graph parity is
  stable.
- Add mirror graph CRDT persistence and state-vector sync.
- Add outbox/idempotency for non-CRDT mutations such as artifacts, imports,
  snapshots, and valuations.
- Design BYOK/local provider replacements for chat, models, OpenCode/terminal,
  TTS, and agent sessions.
- Return explicit capability-disabled responses for hosted-only routes.

Acceptance gate:

- Local graphs work offline without hosted dependencies, hosted graphs keep
  hosted auth semantics, and mirrored graphs expose pending-sync state honestly.

## Immediate Next Slice

The next highest-leverage slice is Phase 0 plus a small Phase 1 increment:

1. Promote explicitly deferred surfaces to accepted only after their listed
   behavior gaps have fixture or live-probe coverage.
2. Add schema snapshots for the currently implemented hosted aliases and MCP
   tools.
3. Make `write_document` either fully accepted as implemented or explicitly
   marked as "present but not semantically accepted" until fixtures pass.
4. Add one thin adapter family where primitives already exist, preferably graph
   metadata/detail/update/delete or remaining navigation folder mutations.
5. Keep timing comparisons in the live harness, with a budget line for common
   reads/writes and separate budgets for ingestion jobs.

This sequence keeps the map honest while still moving the parity frontier. It
also prevents the most expensive kind of rework: exposing broad API/MCP surface
area before schemas, scopes, and CRDT authority rules are nailed down.
