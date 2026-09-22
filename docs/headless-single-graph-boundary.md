# Headless single-graph boundary

`gardend` is a single-graph cell when, and only when, a pure-headless process
starts with a valid `GARDEN_CELL_GRAPH_ID`. Desktop builds and local headless
runtimes without that variable retain the existing multi-graph behavior. A
present but empty, non-Unicode, or invalid identifier fails startup; it never
silently falls back to multi-graph mode.

The graph owner is parsed once before the profile or HTTP socket is opened,
installed as managed process state before journal recovery, published as
`cellGraphId` in the authenticated loopback manifest, and carried by
`LoopbackState`. Ingress, recovery, and background work reuse that same owner;
none can read the environment again or pick an independent default graph.

## Ingress rules

- REST paths and Hocuspocus room paths containing `{graph_id}` must select the
  owner.
- JSON-body graph selectors are checked recursively by `CellGraphJson`; if the
  top-level selector is absent, the owner is injected before the typed request
  is constructed. Recognized source, target, and scene selectors must be
  non-empty strings naming the owner. `newGraphId`, unknown `*GraphId`
  spellings, and non-string selectors fail closed rather than reaching a
  handler's default-graph fallback.
- Job reads, results, and cancellation require durable job metadata naming the
  owner. This includes restore-operation reads/cancellation addressed by
  `operationId`: a matching caller-supplied `graphId` is only a consistency
  assertion, never ownership proof. Each MCP resource tool explicitly binds the
  exact carrier aliases its handler consumes, and contradictory aliases fail
  closed. Profile-wide or legacy jobs without graph metadata are not visible.
- MCP graph tools receive the same checked/injected owner at the single
  dispatcher boundary. Nested arrays/objects, wire target/scene selectors, and
  CRDT payload graph selectors must also name the owner.
- Graph list/create/delete/duplicate/import surfaces, profile reads, workflow
  facades, and opaque local-service proxies are disabled. The equivalent MCP
  lifecycle tools are omitted from `tools/list` and rejected if called anyway;
  disabled REST operations are likewise removed from the authenticated cell
  OpenAPI document.
- Operation-polymorphic surfaces are not trusted merely because they expose a
  `graphId`: MCP `delete(type=graph)` and CRDT `graph.importArchive` are denied.
  MCP `upload_artifact` is disabled because its input is an arbitrary local
  filesystem path. Tools carrying authoritative `services.manage` or
  `services.proxy` effects are disabled so a graph call cannot spawn or proxy a
  profile-local child service.
- Every fresh or recovered durable CRDT operation is checked before a graph
  lease or directory is opened. Recovery validates the complete pending prefix
  before admitting any operation. The time-travel interval scheduler reads only
  the configured owner in cell mode and never enumerates a poisoned/shared
  profile; ordinary desktop/local scheduling and the separate self-heal default
  remain unchanged.
- F4c graph self-heal is additionally constrained by the same startup-bound
  owner. With `GARDEN_SELF_HEAL_GRAPHS=1`, a missing owner graph may heal; a
  different caller-supplied graph id remains not-found and cannot create a
  directory.
- `/health` remains anonymous and does not count as cell activity. Manifest,
  OpenAPI, capability, model/runtime configuration, static entity/vocabulary
  catalogs retain their existing authentication and behavior. Profile-wide
  CRDT timing diagnostics are disabled with the other profile surfaces.

Route-specific scope authorization precedes owner comparison, so a valid but
under-scoped token cannot distinguish owner and foreign paths. The only
anonymous graph-scoped route is signed image delivery; it checks the cell owner
before consulting graph-local token state, and a foreign graph returns the same
`401 missing or invalid image access token` response as an invalid signed URL.

## Error contract

| Condition | REST | MCP |
| --- | --- | --- |
| Foreign graph selector | `404`, `graph not found in this cell` | JSON-RPC `-32004`, same message |
| Foreign or graph-less job | `404`, `graph not found in this cell` | JSON-RPC `-32004`, same message |
| Profile/lifecycle/opaque proxy operation | `403`, operation disabled in a single-graph cell | JSON-RPC `-32004`; lifecycle tool is also absent from `tools/list` |
| Invalid graph-scoped body | `400`/`422` using the existing loopback JSON error envelope | JSON-RPC `-32602` or the tool's existing validation error |
| Unclassified non-graph REST route | `503`, boundary policy missing | n/a |
| Unclassified catalogued MCP tool | startup failure in cell mode | n/a |

The boundary never returns the configured owner in an error.

## Drift gate

Run:

```sh
pnpm parity:cell-boundary
```

The checker classifies every operation in `parity/local-openapi.json`, verifies
that every body-selected graph route uses `CellGraphJson`, maps every job route
to the specific handler containing its ownership check, and explicitly
classifies all tools in `mcp_tool_catalog.json`. A declared `graphId` no longer
auto-classifies a tool: job/operation carriers must be reviewed according to
the resource they actually consume. It also pins the authoritative
local-loopback effect registry byte-for-byte and validates its route/tool
scopes structurally. Service, graph-delete, and graph-import effects therefore
require an explicit deny or operation-specific guard; a top-level `graphId`
alone is not evidence of safety.

The laptop-safe harness runs poisoned two-graph selector, recovery, scheduler,
arbitrary-local-file, and child-service-spawn fixtures. Exact REST/MCP surface
digests plus the authoritative effect-registry digest require explicit review
for every addition; cell startup verifies the same digests. A new non-graph
REST route or MCP tool fails closed until its ownership and effect policy is
explicit.
