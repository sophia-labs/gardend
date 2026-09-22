# Local Storage Format

The local profile lives under the Tauri app data directory:

```text
<app-data>/profiles/default/
```

This is greenfield local state. No users have depended on an older native app
profile, so the implementation does not carry a legacy migration promise yet.
If the format changes before public release, prefer simple rebuild or import
paths over compatibility code.

## Current Layout

```text
<app-data>/profiles/default/
  profile.json
  identity.json
  loopback.json
  loopback-client-tokens.json
  loopback-audit.jsonl
  metadata.turso
  metadata.oxigraph/
  worklog/
    crdt-operations.jsonl
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

## Stores

`profile.json` and `identity.json` are profile records. They are small JSON
files and should be written with the shared storage helpers.

`loopback.json` is a per-run manifest containing local API, MCP, OpenAPI, and
session-token data. It is not a stable credential store.

`loopback-client-tokens.json` stores hashes for named scoped client tokens. It
is written as secret local JSON. Token values are returned only at creation
time.

`loopback-audit.jsonl` is append-only local audit data for token lifecycle and
CRDT write events. It redacts bearer values, token hashes, and payload values.

`worklog/crdt-operations.jsonl` is the durable local write-intent journal for
CRDT operations. Queue acceptance appends a `queued` event containing the full
operation envelope. Runtime completion appends a terminal `succeeded` or
`failed` event. Caller timeout appends `callerTimedOut` but remains
non-terminal, because timeout means the caller stopped waiting, not that the
write intent is invalid. On local-mode startup, non-terminal operations are
re-enqueued for the TypeScript CRDT runtime to drain.

`metadata.turso` is the profile-local Turso graph catalog cache. It stores
`graph_records` rows for local multi-graph listing and update ordering. Graph
directories still keep `graph.json` sidecars for inspection, export/archive
flows, and greenfield cache bootstrap when the Turso catalog is empty.

`metadata.oxigraph/` is the profile-scoped metadata graph. It mirrors the local
graph catalog and is separate from each content graph's `store.oxigraph/`.

Each graph directory owns one content `store.oxigraph/`, graph records,
workspace/document Yjs state, source blobs, and graph-scoped indexes. The
content store separates projection and user RDF authority with named graphs:

```text
urn:mnemosyne:local:graph:{graph_id}:projection:graph
urn:mnemosyne:local:graph:{graph_id}:projection:workspace
urn:mnemosyne:local:graph:{graph_id}:projection:document:{document_id}
urn:mnemosyne:local:graph:{graph_id}:user:rdf
```

Projection materializers own only their projection named graphs. RDF imports
default to the user graph when no `targetGraphIri` is provided, and local RDF
write paths reject reserved projection graph targets.

`jobs/jobs.turso` is the local job cache. It stores job IDs, status, timestamps,
graph ID, job type, and serialized hosted-shaped job records. Large JSON
results spill to `jobs/<job-id>/result.json`.

## Write Discipline

Production code should use shared storage helpers instead of ad hoc writes:

- JSON writes go through the storage helpers.
- Byte writes go through atomic file helpers where practical.
- Secret JSON uses restricted file permissions.
- Durable local work records are written before a request is treated as
  accepted. For CRDT writes, this means appending to the owner-only worklog
  before enqueueing the in-memory drain.
- Projection files are not the write authority. Routes that promise
  projection-visible results must use a targeted CRDT flush/read boundary after
  the durable write; raw CRDT acceptance alone does not imply every cold
  projection has already been materialized.
- Multipart file fields stream into graph-local `pending-uploads/` files before
  adoption by artifact, image, PDF, or RDF handlers.
- Pending upload paths are canonicalized and must stay under the owning graph's
  `pending-uploads/` directory.
- Fresh artifact uploads and PDF fallback ingestion enqueue `pendingOriginalPath`
  references instead of embedding `dataBase64` in the CRDT operation payload.
  The TypeScript parser bridge may still read the pending file when parsing is
  required, but Rust loopback/MCP adapters should not eagerly read and
  base64-copy pending upload bytes before enqueueing work.

The storage discipline checker rejects direct production `fs::write` calls,
buffered multipart `field.bytes().await` reads, direct pending-upload reads
inside enqueue paths, and queued upload `dataBase64` handoffs except for
explicitly counted compatibility exceptions.

## Rebuildable Projections

The following data is derived and may be rebuilt from authoritative state:

- TipTap XML/JSON, DocumentTree, flat blocks, and document RDF from document
  Yjs state. Document RDF is scoped to document projection named graphs.
- Workspace snapshot and workspace RDF from workspace Yjs state. Workspace RDF
  is scoped to the workspace projection named graph.
- Profile metadata RDF from graph catalog records.
- Profile graph catalog cache rows in `metadata.turso` from graph sidecars when
  the cache is absent or empty.
- Semantic block indexes from block projections.

Original source files, Yjs updates, graph/document records, token registries,
and user-imported RDF are not disposable caches.

## Validation

Storage checks are included in:

```bash
pnpm parity:storage
pnpm parity:inventory
```
