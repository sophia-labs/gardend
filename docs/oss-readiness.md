# Source-Available Readiness

Garden is licensed **source-available** under the
[PolyForm Noncommercial License 1.0.0](../LICENSE) — never "open source" /
"OSS" as a category claim (licensing ruling, 2026-09-21). This document
tracks Garden's path from local POC to a credible public release, and is
intentionally conservative: a checked item must have a file, gate, or
implementation artifact that makes the claim true today.

**Two boundaries.** This tracker covers the whole repository, including the
internal Tauri desktop app and its private Shrubbery frontend dependency.
The narrower **public** boundary — what Sophia Labs actually intends to
publish — is the headless `gardend` engine plus `sophia-mcp`; see
[`headless-engine.md`](headless-engine.md) and
[`../EXPORT-MANIFEST.md`](../EXPORT-MANIFEST.md). An item below being "done"
for the full repo does not by itself mean it applies to, or is required by,
that narrower public export.

## Done In This Initial Foundation

- Thin adapters: route groups, MCP endpoint, and Tauri commands are split from
  root wiring into focused modules.
- Contract inventory: `parity/surface-contracts.json`,
  `parity/local-loopback-surface.json`, generated OpenAPI, MCP catalog, and the
  parity inventory gate track accepted vs deferred surfaces.
- Storage documentation: `docs/storage-format.md` names the profile tree,
  `metadata.turso`, graph-local Oxigraph stores, pending uploads, job Turso
  records, original files, and rebuildable projections.
- Security baseline: CSP is non-empty, loopback/MCP scopes are checked, client
  tokens are scoped/audited/expiring, signed image URLs replace the legacy `uid`
  bypass, web imports block private targets, and RDF authority guards reserve
  projection/profile named graphs.
- Pending-file discipline: production multipart uploads stream into pending
  files; fresh upload/PDF CRDT payloads pass `pendingOriginalPath` instead of
  queued `dataBase64`; `parity:storage` rejects direct production writes,
  buffered multipart reads, pending-upload enqueue reads, and new queued upload
  base64 handoffs.
- Service/error boundary: graph catalog, time-travel (13+9+12+9 = 43 fns
  across `time_travel_store.rs`, `time_travel_service.rs`,
  `time_travel_restore_service.rs`, `time_travel_mcp.rs`), and
  jobs/storage/services (62 fns across `local_job_registry.rs`,
  `storage_file_ops.rs`, `semantic_service.rs`, `semantic_embedder.rs`,
  `original_file_service.rs`, `rdf_service.rs`) all use `AppResult<T>` with
  typed `AppErrorKind` mapping at direct loopback edges. Tauri command
  wrappers retain `Result<T, String>` because the IPC layer requires String
  errors; they delegate to the typed `_service` functions. Pattern: see
  `graph_service.rs` reference. Verified by `cargo check` and 310/310 lib
  tests.
- CI readiness: `.github/workflows/native-tauri-prototype.yml` records the
  native prototype gate for frontend build/focused tests, Rust fmt/check/tests,
  and parity inventory.
- Cloud-mode guardrails: focused frontend tests cover hosted-mode Bearer auth,
  removal of local `X-User-ID` shortcuts, ignored local billing tier overrides
  in hosted mode, and preservation of local skip-auth behavior.
- Live parity coverage: `parity:loopback` now drives a self-contained fresh
  graph fixture plus isolated memory/song fixtures, asserts manifest token
  posture, checks named-graph RDF materialization/import, exercises graph archive
  import, and has request timeouts plus optional progress logging for stuck
  route diagnosis.
- Durable worklog seed: `docs/local-worklog-architecture.md` defines the shared
  acceptance model for durable CRDT writes and async large-work. CRDT write
  enqueue now appends an owner-only worklog record before in-memory drain, and
  local-mode startup re-enqueues non-terminal operations for recovery.
- Projection boundary discipline: hosted/loopback adapters that return
  projection-backed responses now use a shared CRDT flush helper, while raw CRDT
  and block-edit hot paths keep the durable/live-authoritative acceptance
  contract. Async CRDT jobs flush projection targets before reporting finished
  job results.
- Async large-work coverage: graph archive import, vault imports, PDF accurate
  fast-text fallback, semantic index UI refresh, and artifact upload/import
  use persisted local job envelopes instead of treating request-path completion
  as the only acceptance boundary. **REST artifact upload and stored-artifact
  import default to async**; sync is opt-in via `Prefer: respond-sync` (see
  `prefers_sync_work` in `loopback_artifact_routes.rs:73` and
  `loopback_artifact_ingest_routes.rs:121`). MCP artifact upload/ingest also
  return job envelopes by default; MCP exposes `get_job_status`,
  `get_job_result`, and `cancel_job` for agent-facing job control. Live parity
  polls the new job result/status links.
- **Semantic model setup runs as a background job**: `prepare_semantic_model_job`
  (see `semantic_service.rs:104` and `semantic_model_prepare_jobs.rs`) returns
  a `LocalJobSubmitResponse`; Settings → Local AI polls progress. The
  synchronous `prepare_semantic_model` command remains as a thin wrapper for
  scripts/tests.
- **Projection flush deferred off the request path**: artifact ingest and
  related routes spawn the post-write projection flush via
  `spawn_deferred_document_projection_flush` (`loopback_artifact_ingest_routes.rs`
  and friends). Reads that need fresh projection opt into a synchronous flush
  via the existing `flush_crdt` MCP/HTTP route.
- **Registry-driven MCP dispatch**: `mcp_tool_dispatch.rs:35` uses
  `lookup(name)` against `mcp_dispatch_registry.rs`'s registry of
  `(name, scope, handler_fn)` records. No string-match table remains.
  `parity:inventory` enforces catalog coverage.
- **Loose-JSON return surfaces typed** (commit `7b8823e0`): MCP block
  payloads, salience score values, hosted document history/projection
  responses, MCP tools list, and artifact-import job envelope all return
  typed `#[derive(Serialize)]` structs. `serde_json::Value` is retained
  only at genuine pass-through boundaries (CRDT block engine inputs,
  workspace heterogeneous unions, MCP tool descriptors loaded from
  catalog JSON), each with `///` rationale. OpenAPI regeneration tightens
  `additionalProperties: false` on `SnapshotResponse`,
  `HostedDocumentSummary`, `ArtifactImportResponse`, `BlocksResponse`,
  `BlockContextResponse`, etc.
- Live parity milestone: the 2026-05-06 `parity:loopback` run passed the full
  local loopback surface, including document description RDF materialization,
  batch register projection, EPUB chapter projection, artifact import/ingest,
  async artifact import, and artifact convert read-after-write behavior.
- **Workspace yDoc persistence race closed** (commit `4b5fe892`): the runtime
  singleton was being instantiated twice in Vite dev mode (static vs dynamic
  imports resolved to two module URLs); fixed by anchoring the singleton on
  `globalThis`. Plus dedupe + supersede on `setGraphId`, `attachFilesystemInFlight`
  coalescing, and reordered `ensureGraphAttached` so the sessionStore subscriber
  no longer double-fires. See `docs/workspace-ydoc-persistence-bug.md` for the
  investigation chain.
- **Rename to Garden + license + publication discipline**: package name is
  `garden` in both `Cargo.toml` and `package.json` (with `private: true`,
  `publish = false`); product name is "Garden" in `tauri.conf.json` (window
  title and bundle); identifier is `dev.sophia.garden`. License is PolyForm
  Noncommercial 1.0.0 (`LICENSE`). Repo extraction `mnemosyne-platform/prototypes/open-core-native-tauri/`
  → standalone `garden/` repo landed 2026-05-08 with all path call-sites
  coordinated.
- **Frontend extracted to `frontend/`, but not self-sufficient**: the
  native-shell adapter code (bridge types, provider glue, `native.html`) now
  lives under `frontend/`, replacing the old sibling `mnemosyne-platform`
  checkout dependency. This is *not* OSS/public self-sufficiency, corrected
  from an earlier claim here (see `CHANGELOG.md`'s 2026-09-22 correction
  note): root scripts, Tauri `frontendDist`/before commands, the release
  workflow, and the native-tauri-prototype validation workflow all still
  fetch the authoritative UI from Shrubbery, a private Sophia Labs repo,
  using a `SHRUBBERY_READ_TOKEN` credential (`scripts/shrubbery-frontend.mjs`,
  `.github/workflows/release.yml:48-65`,
  `native-tauri-prototype.yml:59-77,184-202`). The desktop app is internal
  only; it is not part of the public source-available boundary.
- **Deferred-surface promotion, two passes** (commits `a7acc75f` and the
  follow-up promotion pass): 18 entries in `parity/surface-contracts.json`
  flipped from `deferred` → `accepted` after audits confirmed each had a
  passing probe in `parity/check-loopback-surface.mjs`. First pass (11):
  graph-analytics-facade, graph-rdf-import-facade, document-export-aliases,
  document-ydoc-blob-aliases, navigation-mutations, wire-crdt-mutations,
  mcp-graph-job-adapters, mcp-orientation-adapters, mcp-workspace-management,
  mcp-wire-adapters, mcp-document-and-block-mutations. Second pass (7):
  graph-export-facade, graph-archive-import-facade, graph-vault-import-facade,
  artifact-pdf-accurate-facade, document-crdt-mutations, mcp-artifact-adapters,
  mcp-workspace-mutations. Each promoted entry's note was tightened to
  describe what the probe actually verifies and what (if anything) lives
  separately as a tracked future surface.
- **A2 replay-safety items 10 + 11 closed**:
  - Item 10 (order-derivation, commit `9d15bc38`): the last `?? Date.now()`
    fallback in a CRDT op handler (`putWorkspaceArtifact` line 1891) replaced
    with a strict throw matching the established pattern. Rust
    `normalize_payload_ids` now injects `order` for `workspace.putArtifact`.
  - Item 11 (updatedAt policy, commit `bdfbc040`): mirrors item 10 — Rust-side
    `inject_updated_at_if_absent` derives `updatedAt` from journaled
    `enqueue_timestamp` and injects into payloads for 13 op kinds; 8+ TS
    handlers swept to read `value.updatedAt` with strict throw guards
    instead of calling `Date.now()` / `new Date().toISOString()`.
  - The 11-item `docs/a2-replay-mitigations.md` punch list is now fully
    landed.
- **Parser per-page progress wired** (commit `5300d1ac`): the pymupdf4llm
  Python helper (`scripts/pymupdf4llm_pdf_to_markdown.py`) now loops pages
  and emits `MN_PROGRESS {json}` per page boundary, using `sys.__stdout__`
  to escape the `redirect_stdout(sys.stderr)` context that keeps other
  helper output JSON-clean. Rust `process_utils.rs` already had the parsing
  pipeline; the fix tightened the pipeline so MN_PROGRESS lines update the
  job registry but are stripped from the accumulated stdout buffer (so
  `parse_json_command_output` doesn't choke on interleaved markers). New
  unit test `parser_progress_markers_flow_to_job_record_and_strip_from_stdout`
  asserts both halves of the contract.

## Still Blocked Before a First Public Release

- Installer, signing, notarization, update channels, SBOM, and reproducible
  build notes are not complete. `tauri.conf.json` has basic bundle config; no
  signing step exists in CI. **Correction:** `docs/release-process.md` does
  exist — it documents the current (unsigned, ad-hoc-signed, draft-release)
  desktop pipeline and lists these same open items in its "Open Items Before
  v1" section; it just doesn't yet describe a *finished* signed/notarized
  pipeline.
- 3 surfaces still carry `"status": "deferred"` in `parity/surface-contracts.json`,
  each with a concrete and specifically-scoped blocker:
  - **graph-web-import-facades** — successful clip/YouTube import depends on
    public network reachability, live upstream page shape, caption API
    availability, and SSRF redirect blocking. Promotable with a fixture-server
    harness (stable HTML page + stub captions). Guard and error semantics are
    already live-tested.
  - **artifact-ingestion-and-images** — searchable-PDF and OCR-typed binary
    ingestion depend on the Docling runtime (and bundled OCR back-end) which
    is not yet packaged with the desktop build. Markdown/HTML/EPUB/image
    ingestion paths are already accepted under this same surface.
  - **search-aliases** — the hybrid arm (`/search/hybrid`) is exercised but
    lacks deep assertions on lexical+semantic blending, score ordering, and
    min_score filtering. Promotable with ~1-2 hours of probe extension once
    the prepared local fastembed index is wired into the parity fixture
    setup. Lexical visibility, rematerialize, reindex, and the semantic
    setup-required gate are already accepted.
- In-process retry for polled-but-uncompleted CRDT operations: `crdt_queue.rs`
  has the durable journal and recovery path on restart, but no in-flight
  retry loop with bounded backoff — currently relies on app restart to
  re-enqueue from journal.
- Job-result plumbing still takes `Result<serde_json::Value, String>` as a
  parameter type, propagating loose JSON across 7 callers
  (semantic_model_prepare_jobs, pdf_docling/pymupdf jobs, several loopback_*
  routes). The function-return surfaces have been typed (commit `7b8823e0`);
  converting the parameter type needs a coordinated change to the
  `LocalJobRegistry::finish_existing` / `insert_finished` plumbing.
- Crash, restart, and end-to-end desktop persistence tests are not yet required
  CI gates. `src-tauri/tests/` directory does not exist; the workflow at
  `.github/workflows/native-tauri-prototype.yml` only runs `cargo test --lib`.

## Promotion Rule

Move an item from blocked to done only when the repository contains the concrete
artifact and a command, test, or reviewable file proves it. Do not treat a
passing broad gate as proof for release readiness unless that gate specifically
covers the claim.
