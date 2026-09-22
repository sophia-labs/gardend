# Garden

**Garden is a local-first knowledge graph engine.** Notes, documents,
files, RDF triples, and a SPARQL store all live on your own machine, in
your own profile directory, with no hosted backend.

[![License: PolyForm NC 1.0.0](https://img.shields.io/badge/license-PolyForm--NC--1.0.0-blue)](LICENSE)
![Status: pre-release](https://img.shields.io/badge/status-pre--release-orange)
![Platform: macOS](https://img.shields.io/badge/platform-macOS-lightgrey)

> ⚠️ **Source-available, not open source.** Garden is licensed under the
> [PolyForm Noncommercial License 1.0.0](LICENSE): you may read, run, modify,
> and redistribute it for non-commercial purposes, but this is not an
> OSI-approved "open source" license, and Garden is never described as OSS.
> See [License and notices](#license-and-notices).
>
> **Public boundary.** The desktop app below (Tauri shell + Shrubbery
> frontend) is Sophia Labs' internal product surface, not the public
> boundary: its dev/build/CI require a private Shrubbery checkout and a
> `SHRUBBERY_READ_TOKEN` credential external contributors do not have (see
> [`docs/release-process.md`](docs/release-process.md)). The supported public
> artifact is the **headless `gardend` engine** — this same Rust core, built
> with no Tauri/webview/GUI dependency — plus
> [`sophia-mcp`](https://github.com/sophia-labs/sophia-mcp).
> [`docs/headless-engine.md`](docs/headless-engine.md) is a build/test path
> that needs zero organization credentials. The
> [source-available readiness tracker](docs/oss-readiness.md) records exactly
> what is done and what still blocks a public release of that boundary.

## Contents

- [What Garden is](#what-garden-is)
- [What Garden is not (yet)](#what-garden-is-not-yet)
- [Prerequisites](#prerequisites)
- [Run](#run)
- [Build](#build)
- [Current Contract](#current-contract)
- [Loopback API and MCP](#loopback-api-and-mcp)
- [Frontend Port Scope](#frontend-port-scope)
- [Semantic Scope](#semantic-scope)
- [Semantic Index Scope](#semantic-index-scope)
- [Document Schema Scope](#document-schema-scope)
- [Local Profile Shape](#local-profile-shape)
- [Hocuspocus-Style Document Pipeline](#hocuspocus-style-document-pipeline)
- [Validation](#validation)
- [Docs map](#docs-map)
- [License and notices](#license-and-notices)

## What Garden is

Garden is a Tauri shell with a Lit/TypeScript frontend and a Rust backend.
The backend runs a local loopback HTTP server and an MCP endpoint bound to
`127.0.0.1` on an OS-assigned port. The shell intentionally implements a
narrow local-only slice:

- Tauri desktop shell.
- Rust-owned local profile directory.
- Product-level graph commands exposed through Tauri invoke.
- Durable local graph catalog under the app data directory.
- Durable local document records under each graph.
- Production frontend shell loaded through `frontend/native.html`.
- Production filesystem Yjs workspace state persisted locally, with a materialized workspace snapshot for Rust/API/MCP reads and hosted-shaped workspace RDF in Oxigraph.
- TipTap editor state bound to `Y.XmlFragment('content')`.
- Yjs update bytes persisted as local document state.
- TipTap JSON/XML, DocumentTree snapshots, flat block projections, and RDF materialization.
- Local uploads that create read-only source documents with production `sf_*` metadata.
- Original source file persistence for local view/download flows.
- Oxigraph-backed local RDF datasets.
- Local SPARQL query and update operations.
- Fastembed-backed local block embeddings with a graph-scoped semantic index.
- In-process per-graph Oxigraph store cache, so loopback/MCP activity shares one local store handle instead of reopening the RocksDB database per request.
- No Redis, LocalStack, S3, DynamoDB, hosted auth, or hosted backend API.

## What Garden is not (yet)

- It is not a hosted service. There is no cloud backend, no shared workspace
  across users, and no remote sync. Everything lives in the local profile.
- It is not a published, signed, notarized desktop app yet. Installer,
  signing, notarization, update channels, SBOM, and reproducible-build notes
  are tracked release gates — see [`docs/release-process.md`](docs/release-process.md)
  and [`docs/oss-readiness.md`](docs/oss-readiness.md).
- It is not a complete clone of the hosted Sophia API. Loopback REST and MCP
  expose a deliberate local-native surface, tracked in
  `parity/surface-contracts.json` and `parity/local-loopback-surface.json`.
- It is not a self-sufficient frontend checkout. `frontend/` holds Garden's
  native-shell adapter code (bridge types, provider glue, `native.html`), but
  the authoritative UI components are Shrubbery, a private Sophia Labs repo
  fetched at dev/build time by `scripts/shrubbery-frontend.mjs` using a
  `SHRUBBERY_READ_TOKEN` credential (`.github/workflows/release.yml`,
  `native-tauri-prototype.yml`). External contributors cannot build the
  desktop app end to end; use the headless engine
  ([`docs/headless-engine.md`](docs/headless-engine.md)) instead.

## Prerequisites

> **This section is for the desktop app, which is internal-only.** It needs
> a private Shrubbery checkout and a `SHRUBBERY_READ_TOKEN` credential (see
> [What Garden is not (yet)](#what-garden-is-not-yet) above) that external
> contributors will not have. If you don't have org credentials, skip to
> [`docs/headless-engine.md`](docs/headless-engine.md) — the public,
> credential-free build/test path for the `gardend` engine.

- **Rust 1.88+** — for the Oxigraph-backed RDF store and the fastembed local
  embedding dependencies.
- **pnpm** — managed via `corepack enable && corepack prepare pnpm@latest --activate`.
- **Tauri CLI** — `pnpm tauri` shells out to `cargo tauri`, which is not
  bundled with the Rust toolchain. Install once with
  `cargo install tauri-cli --version '^2.0' --locked`.

A typical first-time clone looks like:

```bash
mkdir -p sophia-labs && cd sophia-labs
git clone git@github.com:sophia-labs/gardend.git
cd garden
cargo install tauri-cli --version '^2.0' --locked
```

## Run

```bash
pnpm --dir frontend install
pnpm tauri dev
```

The repository's root `package.json` declares only scripts, so the install
under `frontend/` shown above is the only one required. `pnpm` will warn
about a missing root `node_modules` directory; that warning is safe to
ignore. `pnpm tauri dev`'s `beforeDevCommand` shells out to
`node scripts/shrubbery-frontend.mjs dev` (`src-tauri/tauri.conf.json`),
which requires the private Shrubbery checkout described above — this command
does not run to completion without org credentials.

If an old Tauri dev window is already running, stop it and restart the Tauri
dev command. The dev server serves `frontend/native.html`, and a running Tauri
process will keep serving its previous URL until it restarts.

## Build

```bash
pnpm tauri build
```

The prototype currently targets the macOS `.app` bundle only. Installer formats such
as DMG are intentionally left for the packaging hardening slice.

### Headless cell (`gardend`)

The same engine also builds as a **GUI-free `gardend` cell** — one binary that
boots the full local engine (loopback REST/MCP, storage, CRDT queue, scheduler,
Oxigraph, semantic index) on Tauri's `MockRuntime`, with no webview or
windowing. It serves two roles: the **per-graph cell** for `platform-next` (one
process per graph, fronted by the gateway, durable plane via
`GARDEN_DURABLE_DIR`), and the **batteries-included backend that `sophia-mcp`
spawns** (`--backend local`).

```bash
./src-tauri/build-gardend-headless.sh
```

See [`docs/headless-gardend.md`](docs/headless-gardend.md) for the cell
architecture, build modes, and how it relates to the desktop app.

## Current Contract

The frontend calls product-level commands:

- `get_capabilities`
- `get_profile`
- `list_graphs`
- `create_graph`
- `list_documents`
- `create_document`
- `read_document`
- `delete_document`
- `read_workspace`
- `save_workspace`
- `save_document`
- `save_original_file`
- `read_original_file`
- `read_pending_upload_file`
- `cleanup_pending_upload`
- `load_rdf`
- `dump_rdf`
- `run_sparql_query`
- `run_sparql_update`
- `get_semantic_index_status`
- `get_semantic_model_status`
- `prepare_semantic_model`
- `refresh_semantic_index`
- `semantic_search`
- `poll_crdt_operations`
- `complete_crdt_operation`

These should remain close to the eventual graph-provider contract. The local implementation is intentionally different internally: profile graph catalog metadata is cached in Turso with JSON sidecars for inspection/export, document manifests own document records, Oxigraph owns the RDF dataset, and the UI calls provider-shaped commands. Hosted-provider routing can later implement the same command surface without forcing the shell to know whether a graph is local or hosted.

The important constraint is surface parity, not implementation parity. Local graphs should keep the same frontend-facing document, workspace, RDF, and artifact operations as hosted graphs even when the local provider uses Rust files, Yjs update blobs, and Oxigraph instead of Redis, S3, DynamoDB, Hocuspocus, or hosted APIs.

The hosted worker/Redis abstraction is a reference for lifecycle semantics, not a dependency target. Local mode should preserve typed operation envelopes, status/result records, bounded waits, graph-store ownership, and timeout cleanup, while replacing Redis streams, leases, S3 snapshots, and worker pods with in-process or file-backed desktop equivalents.

For mutating loopback/MCP parity, CRDT state must remain authoritative. External local API writes should not edit JSON manifests or RDF directly; they route through a local Yjs transaction path, then let manifests, snapshots, RDF, and search indexes update from that state. See [`docs/api-mcp-parity-roadmap.md`](docs/api-mcp-parity-roadmap.md) for the CRDT operation-queue plan and next parity targets.

The first shared CRDT surfaces are `frontend/src/crdt/managed-channel.ts`, `frontend/src/crdt/document-roundtrip.ts`, and `frontend/src/native/workspace-materialization.ts`. Native local workspace/document state now runs through managed Y.Doc channels with serialized transactions, debounced persistence, and explicit flush hooks. Document snapshots are projected from `Y.XmlFragment('content')` into TipTap XML, ProseMirror JSON, DocumentTree, flat blocks, and text, matching the hosted Y.Doc-first materialization path. Workspace snapshots are projected from the graph-scoped Y.Doc into folders, documents, artifacts, wires, UI state, and section trees so Rust/API/MCP reads do not need to parse Yjs directly. Rust also materializes that workspace snapshot to RDF with the hosted ownership split: workspace owns folder/artifact/document metadata and wires; document materialization owns only content subjects under `document#...`. Local graph catalog metadata is projected separately into a profile-scoped metadata graph at `urn:mnemosyne:local:profile:{profile_id}:meta`, mirroring hosted's per-user `:meta` graph without weakening the per-graph content-store boundary. The loopback/MCP write adapter targets those surfaces through a typed operation queue rather than adding direct manifest mutations.

## Loopback API and MCP

When the Tauri app starts, it also starts a local loopback server:

- Binds only to `127.0.0.1` on an OS-assigned random port.
- Writes a per-run bearer token plus API, MCP, and OpenAPI URLs to `<app-data>/profiles/default/loopback.json`.
- Requires `Authorization: Bearer <token>` for all routes except `/health` and signed inline-image URLs.
- Validates browser `Origin` headers for localhost/Tauri origins to reduce DNS rebinding exposure.
- Blocks web import fetches to localhost, private, link-local, multicast, and reserved network targets before each request and redirect.
- Serves inline document images through bearer auth or unguessable per-image access tokens; the legacy `uid` query bypass is rejected.
- The loopback manifest advertises explicit `tokenScopes`, structured `scopeDetails`, and grant profiles for read/write/delete/artifact/RDF/search/MCP surfaces. The native shell can issue and revoke named scoped client tokens from those profiles; client tokens are hashed at rest, expire, and write redacted token lifecycle events to the local audit log. The central CRDT enqueue path also records redacted success/failure audit events for REST and MCP CRDT writes, while the per-run `session-all` manifest token remains as a local compatibility bridge.
- Exposes `/openapi.json` as a generated OpenAPI 3.1 compatibility spec sourced from `parity/local-loopback-surface.json`.
- Exposes `/mcp` as a minimal MCP Streamable HTTP-style JSON endpoint for `initialize`, `tools/list`, and `tools/call`.
- Exposes a local job/status facade for hosted-compatible graph query/update calls. These jobs execute immediately in-process today, but return hosted-shaped job IDs, status records, links, and result endpoints. Job metadata persists in a profile-local Turso cache at `jobs/jobs.turso`, and larger JSON results spill to per-job `result.json` files instead of bloating the status payload.
- Exposes the first hosted-compatible read aliases for documents, navigation, search, and wires. These routes wrap local document records, workspace snapshots, block projections, wire snapshots, and fastembed indexes instead of calling a hosted backend.

See `docs/loopback-security.md` for the current loopback/MCP security boundary,
route and tool scope contract, parity drift checks, Tauri CSP/capability policy,
and remaining hardening work. The broader local-native implementation map lives
in:

- `docs/architecture.md`
- `docs/storage-format.md`
- `docs/mcp-contract.md`
- `docs/oss-readiness.md`
- `docs/release-process.md`

Contributor-facing pre-release docs live beside this README:

- `CONTRIBUTING.md`
- `SECURITY.md`
- `CODE_OF_CONDUCT.md`
- `NOTICE.md`

Backend module split has started inside `src-tauri/src/`: `clock.rs` owns
epoch-millisecond timestamps and duration formatting, `web_fetch.rs`
owns web import URL validation, private-network/SSRF blocking, redirect policy,
and bounded response reads, `storage.rs` owns shared JSON read/write, secret
JSON, and path-display helpers, `ids.rs` owns local ID/title/slug/filename/MIME
safety helpers, `paths.rs` remains a compatibility facade over focused path
modules, `pending_upload_paths.rs` owns graph/profile pending-upload validation/writer
primitives, `pending_upload_service.rs` owns the validated Tauri read/cleanup
bridge for frontend parsers, `multipart_pending_upload.rs`
owns shared multipart field streaming for artifact, PDF, image, RDF, vault archive,
and graph archive uploads,
`profile_metadata_db.rs` and `graph_catalog_store.rs` own the profile-local
Turso graph catalog cache used by multi-graph listing, while JSON graph records
remain sidecars for inspection and archive/export paths. `local_jobs.rs` owns the
Turso-backed local job registry, progress/cancellation records, hosted-shaped
job response envelopes, and result spill files. `profile_rdf_store_service.rs` and
`graph_metadata_materializer.rs` project the profile graph catalog into a
profile-scoped metadata graph. `rdf.rs` owns low-level RDF triple/literal
formatting, subject URI helpers, and RDF format parsing; `rdf_authority.rs`
owns the named-graph authority split for projection and user RDF graphs; RDF
materializers live in narrow `rdf_*` modules. `lib.rs` is now mostly module
wiring and Tauri invoke registration, while loopback/MCP adapters remain flat
modules pending a larger adapter/crate split.

The REST and MCP surfaces are tracked in generated and checked artifacts rather
than hand-maintained README lists:

- `parity/local-loopback-surface.json` is the source registry for local routes
  and MCP tool names/scopes. Rust loads the MCP scope entries from this registry
  for `tools/list` metadata and call authorization.
- `parity/local-openapi.json` is generated from that registry and served live
  through `/openapi.json`.
- `src-tauri/src/mcp_tool_catalog.json` is the local MCP tool catalog used by
  `tools/list`.

Run `pnpm parity:inventory` after
changing route or tool behavior. It verifies local route discovery, OpenAPI,
scope metadata, MCP catalog/dispatch coverage, Tauri security, and storage
discipline.

Run the live loopback surface checker with:

```bash
pnpm parity:loopback
```

The checker also flushes the current workspace CRDT state when a local graph exists, compares workspace snapshot counts with RDF `doc:TipTapDocument`, `doc:Folder`, `doc:Artifact`, and `mnemo:Wire` counts, exercises the projection-backed MCP read adapters (`document_digest`, `get_block`, `query_blocks`, `list_wire_predicates`, and `get_wires`), hits the hosted-compatible document/navigation/search/wire REST aliases when a document exists, writes a hosted-shaped REST document fixture with marks/todo/code fields, verifies search visibility, writes a markdown fixture through MCP `write_document`, reads it back, verifies search visibility, and deletes both fixtures.

Compare the running local loopback against the legacy Kubernetes API with:

```bash
pnpm parity:compare --markdown
```

The comparison harness reads the Tauri loopback manifest, uses `kubectl` to reach `prod/mnemosyne-api` on `127.0.0.1:18080` if a port-forward is not already running, authenticates to the legacy stack through the internal-service header without printing secrets, resolves legacy async job envelopes, and reports status/shape/count/key differences. Useful overrides:

- `SOPHIA_LEGACY_USER_ID=vera`
- `SOPHIA_LEGACY_GRAPH_ID=<graph-id>`
- `SOPHIA_LOCAL_GRAPH_ID=<graph-id>`
- `SOPHIA_LEGACY_BASE=http://127.0.0.1:18080`
- `SOPHIA_LEGACY_PORT_FORWARD=false`
- `SOPHIA_COMPARE_REPEAT=5` or `--repeat=5` to report median request timings
- `--include-semantic` to include hybrid/semantic search probes that may require an index
- `--include-mutations` to add hosted `PUT`/`DELETE` document smoke probes against both stacks. Mutation probes also fetch local CRDT phase timings from `/api/local/crdt-timings` so the markdown report can separate queue, JS/Yjs, document save, RDF materialization, workspace save, and response-readback costs without changing hosted-compatible response bodies.
- `--include-write-fidelity` to enable the canonical REST write/read/search/delete assertions without separately requesting the historical mutation smoke flag. `--include-mutations` implies this.
- `--include-mcp-fidelity` to compare MCP write/read/search/delete probes when both stack bases expose a compatible `/mcp` endpoint. This is optional because the legacy Kubernetes API and standalone `mnemosyne-mcp` are usually deployed separately.
- `--strict` to fail on status errors, unresolved async jobs, or semantic/schema differences. Expected missing-document responses after delete are treated as successful assertions.

Seed deterministic semantic/materialization fixture workspaces in both stacks with:

```bash
pnpm parity:seed-fixtures --markdown
```

The fixture corpus lives under `parity/fixtures/semantic-corpus/` and creates or
reuses graph `semantic-stock-fixtures` with five stable documents covering CRDT
roundtrip, local embeddings, PDF ingestion, loopback API/MCP, and hybrid sync.
Legacy documents are written with `PUT /documents/{graph_id}/{doc_id}`. Local
documents are written through `/api/crdt/operations` using `document.write`, then
materialized into TipTap XML/JSON, DocumentTree, flat blocks, RDF, and snippets.

Useful fixture commands:

```bash
pnpm parity:seed-fixtures --local-only --markdown
SOPHIA_LEGACY_GRAPH_ID=semantic-stock-fixtures SOPHIA_LOCAL_GRAPH_ID=semantic-stock-fixtures pnpm parity:compare --markdown
SOPHIA_LEGACY_GRAPH_ID=semantic-stock-fixtures SOPHIA_LOCAL_GRAPH_ID=semantic-stock-fixtures pnpm parity:compare --markdown --include-mutations --strict
```

Semantic index refresh is intentionally explicit for now:

```bash
pnpm parity:seed-fixtures --local-only --refresh-semantic-index --markdown
```

`--refresh-semantic-index` submits the local async refresh job and polls
`/api/semantic/index/refresh/jobs/{job_id}`. The embedding work is still
CPU-bound on large corpora, but the seeder no longer parks the loopback request
path on the synchronous compatibility endpoint. Tune
`SOPHIA_SEED_JOB_WAIT_MS` and `SOPHIA_SEED_JOB_POLL_MS` for slower local models.

`POST /api/semantic/index/refresh` remains for compatibility with older local
scripts, but new REST, MCP, Tauri, and fixture workflows should use the job
surface so refreshes are pollable and cancellable.

Regenerate the local OpenAPI artifact with:

```bash
pnpm parity:openapi
```

Run the static hosted/local inventory checker with:

```bash
pnpm parity:inventory
```

That checker validates local loopback routes, local OpenAPI output, Tauri
security settings, and storage discipline. When the hosted platform or
standalone MCP source trees are available through `MNEMOSYNE_PLATFORM_ROOT` or
`MNEMOSYNE_MCP_ROOT`, it also scans those surfaces and verifies every
discovered hosted surface has a local-native classification in
`parity/surface-classification.json`.

This is intentionally not a hosted API clone. The loopback server is a second adapter over the local provider surface. The first mutating workspace endpoints use the CRDT operation queue, and the manifest/MCP tool list now expose explicit local scope metadata. Explicit user-approved per-client tokens are still needed before exposing write/delete tools as a product default.

The full hosted API and MCP parity plan lives in [`docs/api-mcp-parity-roadmap.md`](docs/api-mcp-parity-roadmap.md); how outside consumers (Claude / Codex / agents / scripts) talk to Garden is in [`docs/api-mcp-external-integration.md`](docs/api-mcp-external-integration.md). Those plans are the working target for adding hosted-compatible loopback route aliases and expanding MCP beyond the initial POC.

## Frontend Port Scope

The native entrypoint is deliberately part of the production frontend:

- `frontend/native.html` loads `frontend/src/native-main.ts`.
- `native-main.ts` enables `runtimeConfig.nativeLocal` and mounts the normal app shell.
- `frontend/src/native/native-local-runtime.ts` adapts the production session, filesystem, and document stores to the local Tauri provider.
- `frontend/src/runtime/graph-runtime.ts` introduces the first `GraphRuntime`/`RuntimeRegistry` seam so hosted and local runtimes can share lifecycle, graph, document, reconnect, and snapshot calls before full per-graph dispatch lands.
- Sidebar operations now use the same store/controller path as hosted mode, including folder/document drag and drop.
- Local file and folder uploads create real filesystem entries and persisted local provider records instead of prototype-only shell documents.

This keeps native-specific code limited to provider adapters, bridge types,
and platform affordances. The authoritative frontend source itself is *not*
vendored here — it is Shrubbery, fetched from a private repo at build time
(see the public-boundary note at the top of this README).

## Semantic Scope

The RDF/SPARQL slice is backed by Oxigraph rather than a hand-rolled parser. The POC supports local RDF load/dump plus SPARQL query/update operations for the local graph store:

- Query forms: `SELECT`, `ASK`, `CONSTRUCT`, `DESCRIBE`.
- Update forms accepted by Oxigraph's SPARQL Update evaluator, except broad
  named-graph mutations such as `CLEAR ALL`, `DROP NAMED`, variable `GRAPH`
  targets, or direct writes to reserved projection graph IRIs.
- RDF input/output formats exposed in the UI: Turtle, N-Triples, N-Quads, TriG, RDF/XML, N3, JSON-LD.
- Optional target named graph IRI for RDF imports. If omitted, imports target
  `urn:mnemosyne:local:graph:{graph_id}:user:rdf`; reserved projection graph
  IRIs are rejected.
- Optional source named graph IRI for graph-format RDF dumps. Dataset formats
  such as TriG still dump the full dataset unless `sourceGraphIri` is provided.

This is semantic parity for local RDF graph operations, not parity for hosted auth, collaboration, subscriptions, federation policy, or cloud-only storage behavior.

## Semantic Index Scope

Local semantic search is intentionally local-first:

- Embeddings are generated in the Tauri Rust backend with `fastembed`.
- The default model is `nomic-ai/nomic-embed-text-v2-moe` through fastembed's Candle backend.
- Indexed document blocks use the same `search_document:` / `search_query:` prefix convention as the hosted Nomic embedding pipeline.
- Index data is persisted under `indexes/semantic/blocks.json` for the POC.
- The JSON index stores provider/model metadata, block IDs, document IDs, block text, content hashes, and normalized vectors.
- In native-local mode, the global search palette routes semantic block search through the Tauri commands instead of hosted `/search` endpoints.
- The model is not bundled by default. `get_semantic_model_status` reports whether setup is needed, and `prepare_semantic_model` performs the opt-in download/load step.
- Settings > Local AI exposes the same lifecycle: prepare/load the local model, inspect graph index status, and refresh the selected graph index after flushing pending local CRDT state.

This JSON vector store is deliberately replaceable. SQLite FTS5 plus `sqlite-vec`/SQLite `vec1`, LanceDB, or another embedded vector backend can take over the same command surface later without changing the frontend contract.

The default setup flow downloads the selected model into the user's Hugging Face cache and writes a small local setup manifest under `models/semantic/model.json`. A packaged/offline build can still preseed that cache or ship an alternate model pack later, but the default app build does not need to carry the model files.

## Document Schema Scope

Documents are intentionally schema-first in this POC. The local editor uses TipTap Collaboration with Yjs and the production field name, `content`, so the persisted CRDT state is a Yjs update for `Y.XmlFragment('content')`, not an alternate plain-text representation.

The native runtime also persists the production filesystem store as a graph-scoped Yjs workspace document. That workspace document is the local authority for folders, document ordering, artifact entries, wires, source-file metadata, and UI state such as expanded folders. Document manifests remain durable provider records for local file paths and content projections, but hosted-shaped document metadata in RDF is owned by workspace materialization.

On document save, the frontend derives projections from the active Y.Doc and captures:

- `ydocUpdateBase64`: encoded Yjs update bytes.
- `tiptapXml`: `Y.XmlFragment('content').toString()`.
- `tiptapJson`: TipTap/ProseMirror JSON.
- `tree`: DocumentTree snapshot with `fragment`, `block`, `mark`, and `text` nodes.
- `blocks`: compatibility projection for the existing flat document API.

On workspace save, the frontend derives a `workspace.json` snapshot from the graph-scoped workspace Y.Doc and captures folders, documents, artifacts, wires, expanded-folder state, and document/artifact section trees. Rust `read_workspace` and MCP `get_workspace` return that snapshot when present, falling back to a flat document list only before the first workspace flush. The same snapshot is materialized into Oxigraph as `doc:Folder`, `doc:Artifact`, `doc:TipTapDocument`, and `mnemo:Wire` resources using the hosted predicates for title, containment, order, section, source files, and wire endpoints. Workspace projection triples live in `urn:mnemosyne:local:graph:{graph_id}:projection:workspace`.

The Rust provider writes document Yjs updates to `ydocs/documents/<document-id>/update-v1.bin`, keeps sidecar JSON/XML snapshots for inspection, writes workspace snapshots to `ydocs/workspace/workspace.json`, materializes workspace metadata to Oxigraph, and rematerializes the DocumentTree content into Oxigraph using the canonical `http://mnemosyne.dev/doc#` predicates (`childNode`, `siblingOrder`, `nodeId`, `content`, `textContent`, block attributes, annotations, image/math attrs, and preserved extras). Document projection triples live in `urn:mnemosyne:local:graph:{graph_id}:projection:document:{document_id}`.

Local uploads preserve the hosted source-file shape on filesystem entries:

- `sf_storageKey`
- `sf_originalFilename`
- `sf_mimeType`
- `sf_sizeBytes`
- `sf_fileType`

The source bytes are stored under the local document directory and served through `read_original_file`, so the shared original viewer and download flow can operate without cloud storage.

This is still a POC: it does not yet do local multi-process CRDT sync, deep document edit operations from MCP, RDF-to-Yjs rehydration beyond DocumentTree snapshots, provider federation, graph bundle import/export, binary DOCX/EPUB/PDF parsing, or full production UI affordances. Those should be added behind the same provider/document surfaces rather than through a second local-only schema.

## Local Profile Shape

The runtime creates:

```text
<app-data>/profiles/default/
  profile.json
  identity.json
  loopback.json
  loopback-client-tokens.json
  loopback-audit.jsonl
  metadata.turso
  metadata.oxigraph/
  graphs/
    <graph-id>/
      graph.json
      store.oxigraph/
      pending-uploads/
      documents/
        <document-id>/
          document.json
          original/
            <safe-original-filename>
            manifest.json
      artifacts/
        <artifact-id>/
          original/
            <safe-original-filename>
            manifest.json
      images/
        <image-id>/
          original/
            <safe-original-filename>
            manifest.json
      ydocs/
        workspace/
          update-v1.bin
          workspace.json
        documents/
          <document-id>/
            update-v1.bin
            tiptap.xml
            tiptap.json
            tree.json
            blocks.json
      indexes/
        semantic/
          blocks.json
      models/
        semantic/
          model.json
  jobs/
    jobs.turso
    <job-id>/
      result.json
```

This profile directory is the local authority for the POC. It is greenfield
local state; no released local app profile exists, so this implementation does
not carry a legacy migration promise yet. See `docs/storage-format.md` for the
authority/projection split and storage discipline.

## Cell-Owned Document Pipeline

Shrubbery is Garden's authoritative UI and a Yjs sync client. The Rust cell is
the document authority in both the Tauri app and headless `gardend`:

- **Hot tier** — authenticated `/hocuspocus/docs/...` and workspace WebSockets
  apply Yjs updates to an in-process room and atomically persist
  `update-v1.bin` for crash recovery.
- **Cold tier** — the room schedules Rust-owned projection after a 600ms
  document debounce (250ms for workspaces), materializing TipTap/XML, blocks,
  history, and Oxigraph RDF from the authoritative Y.Doc.
- **Mediated writes** — REST and MCP mutations enter the same durable CRDT
  queue and are drained by the in-process Rust executor. The WebView never has
  to poll or complete persistence operations.
- **Force-flush boundaries** — `flush_crdt` and lifecycle operations drain the
  relevant room projections synchronously when a durable boundary is required.

This is intentionally the same authority model locally and in cloud cells;
only the shell and transport topology differ.

The detailed parity story is in
[`docs/api-mcp-parity-roadmap.md`](docs/api-mcp-parity-roadmap.md).

## Validation

The checks below are the **internal desktop** validation gate; `pnpm
frontend:build` and the Shrubbery test commands need the private checkout
and `SHRUBBERY_READ_TOKEN`. For the public, credential-free build/test
contract, see [`docs/headless-engine.md`](docs/headless-engine.md).

Current smoke checks:

```bash
pnpm frontend:build
export SHRUBBERY_DIR=/path/to/shrubbery # for editable dev/test work
pnpm --dir "$SHRUBBERY_DIR" --filter @shrubbery/organism typecheck
pnpm --dir "$SHRUBBERY_DIR" --filter @shrubbery/organism test:run
cargo fmt --check --manifest-path src-tauri/Cargo.toml
cargo check --manifest-path src-tauri/Cargo.toml
pnpm parity:inventory
```

`parity:inventory` includes OpenAPI/surface checks, Tauri security checks, and
the storage discipline gate. The storage gate rejects production direct
`fs::write` calls and buffered multipart `field.bytes().await` reads except for
the counted archive-import compatibility shim.

The Shrubbery revision packaged by Garden is pinned in
`shrubbery-ui.lock.json`. A release build uses an exact clean local checkout
when one is supplied, otherwise it materializes that immutable revision in
Garden's ignored Rust target cache using authenticated GitHub access. CI uses a
private, organization-scoped Shrubbery action pinned to the same immutable SHA.
An explicit mismatched or dirty checkout fails rather than silently packaging a
different UI.

For the hosted cloud-2 canary, use the explicit SPA deployment path rather than
the generic desktop frontend build:

```bash
AWS_PROFILE=terraform-user ./scripts/deploy-shrubbery-canary.sh --dry-run
AWS_PROFILE=terraform-user ./scripts/deploy-shrubbery-canary.sh
```

The older `frontend/deploy.sh canary` command delegates to this same pinned
path; it cannot build or publish Garden's legacy browser frontend.

It builds the pinned Shrubbery revision with the canary gateway/Cognito/chat
configuration, then verifies those values are embedded in the browser bundle
before it can upload. This keeps the desktop's local loopback configuration
separate from the hosted shell and prevents a playground/debug-pane fallback
from being deployed to canary.

For end-to-end parity smokes against legacy (Tauri must be running):

```bash
pnpm parity:loopback
pnpm parity:compare --include-mutations --include-mcp-fidelity --markdown
```

## Docs map

[`docs/README.md`](docs/README.md) is the full, internal documentation index
(it also covers desktop/Shrubbery-only docs and is not part of the public
export set — see `EXPORT-MANIFEST.md`). A quick orientation to what *is*
public:

- **Public headless boundary** — [`docs/headless-engine.md`](docs/headless-engine.md)
  (build/test contract, zero org credentials needed),
  [`docs/headless-gardend.md`](docs/headless-gardend.md) (architecture)
- **Status & roadmap** — [`docs/oss-readiness.md`](docs/oss-readiness.md),
  [`docs/release-process.md`](docs/release-process.md) (the latter documents
  the internal desktop pipeline; kept public for boundary transparency).
  [`docs/garden-v1-roadmap.md`](docs/garden-v1-roadmap.md) is internal-only
  (whole-repo, frontend-inclusive) and not part of the public export.
- **Architecture** — [`docs/architecture.md`](docs/architecture.md),
  [`docs/headless-gardend.md`](docs/headless-gardend.md),
  [`docs/storage-format.md`](docs/storage-format.md),
  [`docs/local-worklog-architecture.md`](docs/local-worklog-architecture.md)
- **API / MCP contract** — [`docs/mcp-contract.md`](docs/mcp-contract.md),
  [`docs/api-mcp-parity-roadmap.md`](docs/api-mcp-parity-roadmap.md),
  [`docs/api-mcp-external-integration.md`](docs/api-mcp-external-integration.md)
- **Security** — [`docs/loopback-security.md`](docs/loopback-security.md),
  [`docs/loopback-threat-model.md`](docs/loopback-threat-model.md)
- **Query engine** — [`docs/sparql-admission-and-cancellation.md`](docs/sparql-admission-and-cancellation.md),
  [`docs/kg-ultra-garden-integration.md`](docs/kg-ultra-garden-integration.md),
  [`docs/local-service-hosting.md`](docs/local-service-hosting.md),
  [`docs/source-sync-offline-contract.md`](docs/source-sync-offline-contract.md)

`docs/a2-ledger-design.md`, `docs/a2-replay-mitigations.md`, and
`docs/replay-classification/` are internal-only: they audit **frontend**
TypeScript CRDT handlers (`frontend/src/crdt/`,
`frontend/src/native/native-local-runtime.ts`), which live outside the
public boundary.

## License and notices

Garden is licensed under the [PolyForm Noncommercial License 1.0.0](LICENSE).
Non-commercial use, modification, and redistribution are permitted under those
same terms. Commercial use requires a separate agreement with the maintainers.
This is a **source-available** license, not an OSI-approved open source
license — do not describe Garden as "open source" or "OSS".

- [`LICENSE`](LICENSE) — full license text
- [`NOTICE.md`](NOTICE.md) — required notice and third-party attribution policy
- [`THIRD-PARTY-RUST.md`](THIRD-PARTY-RUST.md) — third-party Rust crate
  license inventory for the headless engine closure
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — contribution scope and validation gate
- [`SECURITY.md`](SECURITY.md) — how to report vulnerabilities
- [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md) — collaboration standards
- [`CHANGELOG.md`](CHANGELOG.md) — release history (none yet)
