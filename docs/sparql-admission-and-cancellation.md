# Gardend SPARQL admission and cancellation

Status: P0 query slice implemented; update cancellation remains a versioned
engine seam, described below.

## Why this exists

The external SPARQL surface previously had three independent failure modes:

1. Axum and MCP handlers called synchronous Oxigraph evaluation directly on a
   Tokio worker.
2. `/graphs/query` and `/graphs/update` completed that work synchronously,
   inserted an already-finished local-job record, and only then returned
   `202 Accepted`. Polling and cancellation were a facade.
3. The collector's row ceiling bounded response materialization but did not
   supply a deadline or cancellation authority to the query engine.

One expensive query could therefore pin a runtime worker indefinitely; several
could materialize concurrently; graph deletion or restore could overlap an open
store; and a client-visible timeout did not prove that engine work had stopped.

## Current external surface

| Surface | Query | Update |
|---|---|---|
| `POST /graphs/query`, `/api/graphs/query`, `/api/sparql/query` | Shared admission, blocking executor, graph lease, server-clamped deadline and row/triple ceiling, real Oxigraph cancellation, inline `200` | — |
| `POST /graphs/update`, `/api/graphs/update`, `/api/sparql/update` | — | Shared admission, blocking executor, graph lease, inline `200` after the real commit/failure |
| MCP `sparql_query`, `query_graph` | Same controlled query path; accepts `timeoutMs`/`timeout_ms` and `maxRows`/`max_rows` | — |
| MCP `sparql_update` | — | Same controlled update path |
| Tauri IPC `run_sparql_query`, `run_sparql_update` | Legacy synchronous desktop path | Legacy synchronous desktop path |
| Internal materializers/Emporium/workflow helpers | Deliberately outside external caps; many require full result sets | Deliberately outside external admission |

The generic `/graphs/jobs/{id}` routes remain for job records, but SPARQL no
longer manufactures a completed job merely to return 202. Other graph
handlers' job semantics are outside this slice and still need their own sweep.
The desktop workspace client now awaits the inline update result; latency is
unchanged because the old handler already finished the update before replying.

## Admission contract

`SparqlAdmission` is installed with the ordinary Tauri managed state and owns
one semaphore shared by external reads and writes.

| Setting | Default | Semantics |
|---|---:|---|
| `GARDEN_SPARQL_MAX_CONCURRENCY` | `2` | Process-wide external query + update permits |
| `GARDEN_SPARQL_DEFAULT_TIMEOUT_MS` | `30000` | Query deadline when the caller omits one |
| `GARDEN_SPARQL_MAX_TIMEOUT_MS` | `120000` | Hard server ceiling on requested query deadlines |
| `GARDEN_SPARQL_MAX_ROWS` | `50000` | Hard server ceiling on materialized SELECT rows or CONSTRUCT/DESCRIBE triples |

The query deadline includes semaphore wait and graph-lease wait. Evaluation
runs in `spawn_blocking`. A deadline fires Oxigraph's
`sparql::CancellationToken` and the handler waits for the blocking worker to
unwind before returning 504; it never detaches an engine and labels that
"timed out." Dropping the request/MCP future also fires the same token. The
worker itself retains the permit and graph lease until it really exits.
The seed/reconciliation walk checks that token before filesystem work and
between graph, workspace, and document projection units. A single document
materialization is still atomic; cancellation stops before the next unit and
leaves the durable seed marker stale so a later admitted operation repairs any
partial projection work.

Reaching the collector ceiling also fires the engine token before dropping the
iterator. Truncation remains explicit in the result warning. The ceiling is not
presented as a substitute for deadline cancellation.

`ensure_graph_store_seeded` may perform an incidental projection repair before
a read. It precisely calls `mark_rdf_store_written` after a successful reseed.
On a successful external query, the graph lease therefore declares its
possible RDF writes self-tracked; ordinary reads no longer make every query
look like a write and trigger a redundant durable-plane flush. Any error or
panic retains the lease's conservative dirty-on-drop behavior.

## Graph lifecycle and restore

The external worker holds `GraphPersistenceCoordinator` from before
seed/open through evaluation and result collection/commit. This serializes it
with graph deletion/recreation and closes the open-store teardown race.

Restore previously used only `RestoreGuardState`, which blocks new CRDT
enqueues but did not serialize direct SPARQL or graph deletion. It now:

1. engages an owned restore guard before waiting (so no CRDT or admitted
   SPARQL update can enter in the acquire window);
2. asynchronously acquires the same graph persistence lease;
3. moves both leases into one blocking worker for backup, apply, RDF rebuild,
   verification, and rollback;
4. captures the pre-restore backup through a non-reentrant
   `capture_restore_point_with_lease` path.

Ordinary restore-point capture now takes one graph lease around the whole
bundle instead of reacquiring it once per document. That also makes the bundle
a coherent graph-scoped capture. Its document reads use the existing
non-reentrant headless self-heal body, so a rare ghost document cannot try to
reacquire the graph lease and deadlock the capture. Successful captures
declare possible RDF writes self-tracked—not read-only—because that rare
self-heal may materialize a document and precisely marks its own store write.

## Oxigraph 0.5.9 cancellation audit

Garden is pinned to Oxigraph `0.5.9` / spareval `0.2.6`.

- `SparqlEvaluator::with_cancellation_token` is a real engine facility.
  spareval checks it while scanning datasets and surfaces
  `QueryEvaluationError::Cancelled`.
- Query SELECT/ASK/CONSTRUCT/DESCRIBE evaluation can therefore be cancelled
  cooperatively and is wired in this slice.
- Query-pattern updates (`DELETE/INSERT ... WHERE`) use spareval internally
  and inherit some cancellation checkpoints.
- Pure `INSERT DATA` and `DELETE DATA` loop over parsed quads without consulting
  the token.
- `CLEAR`/`DROP` call storage bulk operations without a token.
- `LOAD` iterates an RDF parser without a token (Garden's Oxigraph build does
  not enable the optional HTTP client, so remote `LOAD` is unavailable).

Because cancellation coverage is not uniform, this patch does **not** accept an
update deadline. Returning 504 while a pure data update continues and later
commits would create an ambiguous write and invite unsafe retries. HTTP
`timeout_ms` and MCP `timeoutMs`/`timeout_ms` on an update are rejected rather
than silently ignored. Query-only row controls are likewise rejected on
updates.

## Remaining phases

### P0.2 — transaction-safe update cancellation

Choose one of these explicit implementation boundaries:

1. Carry a Garden-maintained Oxigraph patch that checks the evaluator token
   before every update operation and periodically inside data/load/clear loops.
   Cancellation must abort the owning transaction before commit.
2. Upstream that behavior and upgrade only after per-operation tests prove it.

Required proof matrix:

- pre-cancel and mid-flight cancel for `INSERT DATA`, `DELETE DATA`,
  `DELETE/INSERT WHERE`, `CLEAR`, `DROP`, and every enabled `LOAD` shape;
- the update returns a cancellation error;
- zero partial quads commit;
- cancellation of one update does not poison the store;
- cancel-vs-commit has one terminal outcome, never "cancelled and committed."

Only after that proof should updates accept a deadline or become cancellable
jobs.

### P0.3 — remaining direct entrypoints

- Convert Tauri IPC query/update commands to async wrappers over the same
  admission path without routing internal materializers through external caps.
- Give direct REST/MCP `rdf_load` and `rdf_dump` a separate bulk-I/O admission
  policy, graph lease, input/output byte ceilings, and blocking/streaming
  execution. They remain synchronous and unbounded in this slice; folding them
  into the row-oriented SPARQL policy would conceal rather than solve that
  different resource shape.
- Inventory internal arbitrary-SPARQL helpers and distinguish trusted bounded
  queries from user-controlled strings. Any user-controlled alias must enter
  admission.
- Add a per-operation query/update text-size ceiling before parsing. The
  generic loopback body limit is an upload limit and is too broad to be a
  parser-memory policy.
- Make a single large document projection cooperatively cancellable or
  memory-budgeted; the seed walk now stops between projection units, but not
  inside one materializer transaction.

### P1 — throughput and memory

- Replace the exclusive graph coordinator with a generation-aware read/write
  lease: concurrent bounded reads, exclusive write/restore/delete.
- Stream SPARQL JSON/N-Quads or page server-side instead of materializing up to
  50,000 `BTreeMap<String, String>` rows.
- Split read and write permit pools if update latency is starved by queries,
  while retaining a small shared memory budget.
- Record admission wait, engine time, materialization rows/bytes,
  cancellation reason, and peak RSS into the Observatory cell. Those fields
  should determine production concurrency and row defaults rather than guesswork.

## Static invariants

Run:

```sh
bash scripts/check-sparql-admission.sh
```

This checks that every direct REST alias and the three raw MCP tools reach the
controlled path, SPARQL handlers no longer create finished jobs or return 202,
the query evaluator receives a cancellation token, update deadlines remain
explicitly refused, restore shares the graph lease, and the MCP catalog
advertises the clamped query controls. It also verifies the generated OpenAPI
and response-envelope snapshots describe the new inline contract.
