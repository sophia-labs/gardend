# Hybrid Sync Runtime

Hybrid runtime support means a graph can be local, hosted, or synchronized
between both providers. The application should route through a graph-provider
contract rather than scattering `if local then filesystem else cloud` decisions
through the UI. A local graph can still expose hosted-shaped reads, and a hosted
graph can eventually hydrate local state for offline work.

## Provider Boundaries

- Workspace state includes folders, document placement, artifacts, wires, and UI metadata.
- Document state includes collaborative content, TipTap projections, block snapshots, and RDF content triples.
- Semantic state includes model provider choice, index timestamps, document coverage, and vector storage.
- Sync state includes dirty ranges, remote revision cursors, conflict records, and retry state.

Keeping these boundaries explicit prevents the local implementation from
copying cloud secret sauce such as Redis streams, S3 paths, LocalStack-specific
assumptions, or backend-only worker leases. The local implementation can use
files, SQLite, Oxigraph, or in-process queues as long as the same provider
contract remains visible to frontend and MCP callers.

## Offline Flow

When the network is unavailable, local writes should continue to update the
workspace Y.Doc and document Y.Docs. Materialized snapshots feed navigation,
semantic search, RDF query, and MCP reads. When connectivity returns, a sync
adapter can compare provider cursors and upload compact Yjs updates rather than
diffing rendered HTML or raw markdown.

## Conflict Policy

The first policy should prefer CRDT merge where possible and produce explicit
conflict records where provider semantics diverge. Folder moves, source-file
metadata, and graph-level settings may need custom merge rules. Semantic indexes
should be rebuilt from materialized state after sync rather than treated as
authoritative sync payloads.

Search terms for this fixture include `provider cursors`, `offline work`,
`graph-provider contract`, `custom merge rules`, and `semantic indexes should be
rebuilt`.
