# Swarm analysis store: immutable, masked read snapshots

Many agents often need to read one loaded RDF and vector corpus at once, each with some named graphs hidden from it. A held-out evaluation is one example: each agent excludes the graph its own question came from. The existing graph surface cannot serve that well. Stress round 3 measured a 10,217 ms p95 wait in the per-graph write queue.

So swarm reads never:
- enqueue a CRDT operation;
- wait for a document projection;
- take the hot-write lease;
- use the per-graph mutation queue.

This is a separate analysis read surface. It does not change existing document, deletion or durability behaviour.

**Local profiles only.** `swarm_load` reads packages from host paths. All six swarm tools are therefore denied in cell mode (`mcpProfileDenied`), and the boundary validator refuses a policy that allows `swarm_load` on a cell. On a shared box, a host-path read would reach other graphs' files.

## Publication and consistency

**One snapshot per graph.** Each graph directory has a `SwarmCell`, which holds a short-lived `RwLock` around an `Arc<Snapshot>`.

**Import and publication:**
- A bulk import builds a fresh Oxigraph store and a validated vector index outside that lock. It commits the bulk load before validating, because quads loaded in bulk are invisible until commit.
- Publication swaps one `Arc`.

**Reads:**
- A read clones the `Arc` under the lock and releases the lock before parsing, searching or walking.
- During an import, readers keep the complete previous generation. After publication they see the new RDF and vectors together.

**Saved queries:** the saved-query maps are separate immutable `Arc`s, so updating one never touches the RDF or the embeddings.

**Source packages:**
- They are read-only. `swarm_load` takes `rdf_path` (N-Quads) and `embeddings_manifest_path`, and validates and builds the full snapshot before publishing it.
- The cell records the canonical paths in its graph-local `swarm/<sha256-of-graph-incarnation>/source.json`, so a restart can reload the package.
- The manifest's SHA-256 is pinned, and the manifest in turn pins the SHA-256 of the RDF and of each vector file. A reload rejects changed sources instead of serving a different corpus under the same generation.
- Operators must keep source packages unchanged.
- No existing source file is overwritten or deleted.

This is not a replacement for the CRDT authority or its recovery log.

## Package contract

**Manifest:**

```
{schema_version: 1, rdf, rdf_sha256, vectors: [{file, sha256, dimensions, rows: [{id, graph, kind, text}]}]}
```

- Each `file` is a relative path inside the manifest directory. It holds exactly rows × dimensions little-endian float32 values.
- Every quad must be in a named IRI graph, and every vector row's `(id, graph)` must appear in the RDF.

**Rejected:**
- nonfinite or zero vectors;
- inconsistent dimensions;
- duplicate rows;
- paths outside the package;
- orphan vector rows.

**Normalisation:** vectors are normalised on import, and query vectors on search. `kind` is a caller-defined class label.

## One masking policy, applied before evaluation

`exclude_graphs` is a list of validated graph IRIs that applies to one request only.

- **Semantic search:** removes excluded records before ranking and top-k. An optional `allowed_ids` narrows the ranked corpus further; an empty list returns no results.
- **Traversal:** checks each edge's provenance before expanding or counting it.
- **SPARQL:**
  - excluded graphs are removed from both the named and the default dataset selections, including explicit `FROM` and `FROM NAMED`;
  - the default dataset is the union of the visible named graphs;
  - counts and aggregates run inside that dataset, and no unmasked store-size metadata is returned;
  - `SERVICE` is disabled.

The policy applies identically to raw and saved SPARQL.

## MCP surface (local profiles)

| Tool | What it does |
|---|---|
| `swarm_load` | Publish RDF and embeddings from a local package. |
| `swarm_query` | Bounded SPARQL SELECT/ASK over a masked snapshot. |
| `swarm_save_query` | Persist a named SELECT/ASK template in the graph directory. |
| `swarm_query_named` | Run a saved template with RDF-term parameters. |
| `swarm_semantic_search` | Rank imported embeddings against a supplied query vector, with no model call. |
| `swarm_traverse` | Bounded outgoing RDF traversal, with graph masking on every edge. |

**Saved-query parameters:**
- Placeholders are written `{{name}}`.
- Each parameter is an object with exactly one of `iri` or `literal`, encoded with Oxigraph's RDF-term escaping.
- Placeholders must sit in RDF-term positions. Placeholders inside strings, IRIs or comments are rejected, so surrounding quotes cannot defeat the escaping.

Embedding the query text is the caller's job.

**Registration:**
- The tools appear in dispatch, the catalog, the effect and scope registry, and the cell boundary policy (as denied).
- Reads use the existing read scopes; loading and saving a query need RDF write scopes.

## Validation

**Policy test:** `swarm_mcp_names_have_dispatch_catalog_scope_and_cell_policy` checks registration and that all six tools are denied on cells.

**Data-backed test:** `swarm_real_export_masking_and_publication` is ignored by default, because it needs a real package. It checks:
- SPARQL counts against the parsed source quads;
- masking of direct, union and `FROM` datasets;
- semantic membership;
- traversal edge provenance;
- concurrent holdouts;
- atomic publication, and reads while the import lock is held.

Run it with:

```sh
GARDEN_SWARM_FIXTURE_DIR=<package dir> scripts/buildbox test swarm_real -- --ignored
```

`GARDEN_SWARM_MASK_GRAPH_PREFIX` optionally limits which named graphs the masking checks use.

**Limits:**
- Source packages must be kept for restarts.
- Snapshots take memory during replacement.
- Arbitrary SPARQL has a result cap and a cancellation deadline.
- No performance claim is made yet; a concurrency harness is still to run.
