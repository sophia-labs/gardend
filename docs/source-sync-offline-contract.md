# Source synchronization and offline authority

**Status:** implemented and verified for the replicated graph data plane
(2026-07-30).

This document is the Garden-side contract behind Shrubbery's complete offline,
sync-later graph mode. It describes the authoritative cell behavior. The
client architecture and full distributed-truth audit live in Shrubbery:

- `docs/design/offline-sync-later.md`
- `docs/design/offline-distributed-truth-audit.md`

## Purpose

HTTP caches and opportunistically persisted Y.Docs are not a complete offline
protocol. A client needs to know:

- which graph and document lifetimes its bytes belong to;
- whether it has a complete source snapshot rather than a partial collection
  of cache hits;
- how every locally accepted mutation merges or conflicts;
- whether a retry is a duplicate of an accepted operation;
- which data is source authority and which data is a rebuildable face.

Garden therefore exposes an identity-fenced source synchronization layer above
storage and below Shrubbery Surfaces.

```text
Shrubbery source mirror
  source_pull  <---- complete, content-addressed source epoch
  source_push  ----> stable typed operations, delivered at least once
  source_rebuild ---> destructive replay of disposable projections
                         |
                         v
Garden source ledger + graph/document authorities
  CRDT | event log | current state | derived/baseline
```

The implementation is centered in `src-tauri/src/source_sync.rs`. The REST and
MCP surfaces call the same service, and both desktop Garden and headless
`gardend` use the same engine.

## Authoritative identity

Every source request is bound to:

- `graphId`;
- the durable graph incarnation;
- stable operation IDs for authored intents;
- a document incarnation for document-body and lifecycle operations.

Graph and document creation may carry client-minted UUID incarnations. An
exact replay resumes the original lifetime. A different incarnation at the
same human-readable ID conflicts. Deletion requires the expected incarnation,
so a delayed client cannot delete a same-ID replacement.

Document incarnations live in `.incarnation-id` inside the authoritative
document directory. The sidecar is created with `create_new`, synced before it
is published, and disappears with the old document directory. Reading or
opening a room never recreates a missing directory.

An empty document is represented by a canonical decodable Y.Doc update. Zero
bytes or a missing body are never promoted to authoritative emptiness.

## Source ledger

Each graph lifetime owns an atomic `source-sync/ledger.json`. The ledger records
the canonical operation, its content digest, accepted revision, durable status,
outcome, and any repairable projection-effect error.

Accepted operations have one of these results:

1. apply exactly once;
2. remain accepted while a failed effect is retried;
3. expose an explicit semantic conflict;
4. reject before acceptance because the graph or object lifetime is stale.

Duplicate delivery returns the same content-bound receipt. Reusing an
operation ID for different content is rejected.

The current operation vocabulary is:

- versioned current-state candidate;
- current-state conflict resolution;
- event-log append;
- memory append;
- valuation event;
- retraction event;
- document create/delete/recreate;
- workspace Y.Doc update;
- document Y.Doc update;
- mediated CRDT command;
- graph metadata update.

Untyped SPARQL UPDATE remains an online administrative escape hatch. It does
not have a general offline merge law and is not accepted as a substitute for a
typed source operation.

## Merge and conflict laws

### Workspace and document CRDT

Y.Doc updates merge through `yrs`. A document update must name the current
document incarnation. The source ledger serializes acceptance with destructive
graph/document lifecycle operations and room writes.

### Event sources

Event-log, memory, valuation, and retraction operations use stable event
identity. Replicas merge by set union and fold deterministically. At-least-once
delivery is idempotent.

### Current-state sources

A current-state candidate includes the version it observed. Concurrent
candidates sharing an observed base become one explicit conflict object unless
the Meaningful Object vocabulary supplies an executable reconciliation rule.
Arrival order does not silently select truth.

### Derived sources and projections

RDF, semantic materializations, workspace/document projections, and other
faces are disposable. They rebuild from source operations and explicitly
captured legacy baselines. A rebuild must preserve source-set equality while
replacing the derived projection.

## Complete source pull

`source_pull` returns an explicit complete bundle, not a best-effort list. Its
manifest is closed over every included payload member:

- graph metadata;
- workspace Y.Doc;
- every document Y.Doc and incarnation;
- current-state objects and conflicts;
- event, memory, valuation, and retraction sources;
- operation receipts and source registry;
- content-addressed original resources;
- document history;
- semantic corpus and capability metadata;
- legacy RDF/value-store baselines;
- the current RDF projection as an epoch-bound acceleration artifact.

Each member has a digest. The manifest has a digest. The epoch is:

```text
graph-incarnation : source-ledger-revision : complete-manifest-hash
```

The manifest hash is required because authoritative state can change through
an existing Garden path that does not append a source-ledger operation. Such a
change must still invalidate every epoch-derived client face.

## Source push atomicity

`source_push` accepts at most 1,000 operations in one batch. Validation occurs
before acceptance, so an invalid operation cannot leave an accepted prefix.
The response contains exactly one receipt per input operation and binds:

- graph ID and graph incarnation;
- operation ID;
- canonical operation digest;
- accepted revision;
- status and outcome.

Individual effects that fail after source acceptance remain durable and
repairable. They are not relabeled as semantic conflicts. The next replay,
rebuild, or explicit repair can finish the effect and clear the stale error.

## Integration surfaces

Garden publishes the protocol through:

- MCP tools `source_pull`, `source_push`, and `source_rebuild`;
- the local authenticated REST contract used by Shrubbery;
- graph create/delete and document read/room paths carrying lifetime fences;
- the parity manifest and MCP tool catalogue.

The hosted gateway may proxy cell source routes generically, but graph lifetime
creation/deletion has an additional gateway contract because the tenancy claim
exists outside the cell. See platform-next's
`docs/offline-graph-lifecycle.md`.

## Failure semantics

| Failure | Required behavior |
|---|---|
| request lost before acceptance | client retries the same operation |
| acknowledgement lost after acceptance | duplicate receipt, no duplicate effect |
| projection effect fails | operation remains accepted and pending repair |
| invalid member in batch | reject the whole batch before acceptance |
| stale graph/document lifetime | reject without touching the replacement |
| direct source change at same ledger revision | manifest and epoch change |
| process death after durable acceptance | cold disk hydration preserves source and receipt |
| destructive rebuild | source set is unchanged; projections are reproduced |

## Verification

The release Gardend binary used by the complete 2026-07-30 proof was:

```text
/Users/vera/dev/sophia/garden/src-tauri/target/release/examples/gardend
sha256 da328259bbd4495e84ad5e978f740ce2b1981c6130fa457074db008b9d75a3b3
```

The cross-repository Shrubbery gate exercises:

- Chromium and WebKit persistent clients;
- Node Yjs and MCP writers;
- both reconnect orders and duplicate delivery;
- process death, cold disk hydration, and lost acknowledgements;
- graph/document delete and same-ID recreation;
- three deterministic five-client chaos seeds;
- destructive source replay and projection equality.

Garden-local checks for this slice include:

```sh
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo test --manifest-path src-tauri/Cargo.toml \
  --no-default-features --features headless --lib source_sync
cargo test --manifest-path src-tauri/Cargo.toml \
  --no-default-features --features headless --lib document_incarnation_store
./src-tauri/build-gardend-headless.sh
```

The complete functional gate is run from Shrubbery:

```sh
pnpm --dir apps/organism verify:offline-distributed-truth
```

## Scope boundary

This protocol establishes fully offline behavior for replicated graph sources
and their locally available faces. It cannot reveal a concurrent gateway ACL
change through a partition, and it cannot execute an external provider whose
implementation or inputs are absent locally. Those are explicit non-local
boundaries, not source-mirror incompleteness.
