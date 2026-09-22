# Local Worklog Architecture

Durable writes and async large-work are the same architectural problem: local
callers need an acceptance boundary that survives UI stalls, app restarts, and
long-running work.

The local runtime should not treat "queued in memory" as accepted work. It should
persist an intent record first, then let a worker advance that record through
observable states.

## Work Classes

The local runtime has two work classes today:

- CRDT write operations: document/workspace mutations that must be applied by
  the Yjs authority in the frontend runtime.
- Local jobs: imports, PDF/runtime work, semantic indexing, restore, graph
  export/import, and other work that can run longer than a request.

They should converge on one rule:

```text
accepted = durable work record exists
```

The worker may be the TypeScript CRDT runtime, a Rust thread, a Python helper, a
semantic indexer, or a future sidecar process. The adapter should not pretend
the work is safe until the record is on disk.

## Current Implementation

CRDT writes now append owner-only JSONL records to:

```text
<app-data>/profiles/default/worklog/crdt-operations.jsonl
```

Each queued operation records:

- schema version
- event ID
- timestamp
- operation ID
- status
- full operation envelope

Completion appends a terminal `succeeded` or `failed` event. Caller timeout
appends `callerTimedOut`, but that is not terminal: the operation remains
recoverable because timeout means the caller stopped waiting, not that the local
write intent is invalid.

On local-mode startup, the runtime scans the journal and re-enqueues operations
that have a queued event but no terminal completion event. Recovered operations
are detached from any old caller response channel and are drained by the normal
frontend CRDT runtime.

Local jobs already persist records under:

```text
<app-data>/profiles/default/jobs/
```

The job registry owns status, progress, cancellation, result spill files, and
hosted-shaped job responses. Heavy work should move toward this path instead of
synchronous loopback request handling.

Large CRDT-backed request paths now share a small Rust job facade:

- graph archive import and Obsidian/Notion/Roam vault import return `202`
  envelopes immediately and finish through `/graphs/jobs/{job_id}/result`;
- PDF accurate ingestion returns a queued job even when the effective engine is
  the local `pdf.fast-text` fallback;
- generic artifact upload and stored-artifact import support
  `Prefer: respond-async` for callers that can consume job envelopes while the
  hosted-compatible default response remains available;
- MCP `upload_artifact` and `ingest_artifact` return persisted local job
  envelopes by default, with MCP `get_job_status`, `get_job_result`, and
  `cancel_job` as the agent-facing job control surface;
- the document switcher uses the semantic index refresh job API rather than the
  synchronous native command.

Projection visibility is a separate boundary from write acceptance. The
authoritative write lands in Yjs state first; workspace/document JSON, RDF,
search, and navigation views are read-model projections. The shared
`crdt_projection_flush.rs` helper now marks the hosted/loopback surfaces that
promise read-your-write behavior:

- hosted document, entity, navigation, wire, batch, web-import, artifact import,
  artifact convert, duplicate, and MCP workspace adapters flush before returning
  projection-backed responses;
- async CRDT jobs flush known workspace/document projection targets before
  marking the job complete, so polling a finished job is a read-after-write
  boundary;
- raw CRDT routes and block-edit hot paths keep the cheaper contract: accepted
  means durable/live-authoritative, while cold projections may settle through the
  normal debounced flush unless a caller explicitly asks for `crdt.flush`.

The 2026-05-06 live loopback parity run passed after this distinction was made
explicit. Its timings showed the atomic workspace mutations were usually small,
while `crdt.flush` was the repeated synchronous cost. That validates the split:
keep hot writes durable and cheap, and force projection materialization only at
adapter boundaries that actually promise a projection-visible result.

## Replay Strategy

Recovery re-enqueues operations whose journal records show a queued event with
no terminal completion. For recovery to be safe, every operation kind must
converge under replay — applying it twice (once before the crash, once after
recovery) must produce the same observable state, or must be guarded so the
second application is a no-op.

The full audit is at:

- `docs/replay-classification/v2/FRAMEWORK.md` — taxonomy definitions and
  adjudication rules
- `docs/replay-classification/v2/*.yaml` — refined per-kind records (28 kinds,
  written by a 28-agent first-order observation pass and a second-order
  refinement pass)
- `docs/replay-classification/*.yaml` — first-pass records, retained as
  evidence trail
- An independent second-order review adjusted 5 verdicts and surfaced 3
  cross-cutting patterns; conclusions below incorporate that review

### Taxonomy

- **A — Convergent.** Replay produces zero observable state divergence. Either
  a pure Y.Doc CRDT mutation on a caller-supplied stable identity, or external
  side effects that are idempotent by construction (overwrite-by-key,
  no-op-on-missing).
- **B1 — Identity hazard.** Handler generates a new identity when the payload
  omits one. Replay creates a duplicate entity.
- **B2 — Metadata drift.** Replay produces semantically equivalent state with
  non-load-bearing metadata that differs across attempts. Typically
  `updatedAt = Date.now()` or a revision counter that increments per call.
- **C — Non-convergent side effect.** Replay produces genuinely divergent
  state that cannot be reconciled by Y.Doc properties or simple
  overwrite-by-key. Offset-based text mutations, resource-consuming filesystem
  operations that fail on second attempt, multi-step external operations
  without idempotency guards.

Several kinds are dual-classified (`B1 + B2`). The primary class drives the
dominant mitigation; the secondary class points at additional drift that
should be addressed in the same fix.

### Per-kind classification

| Kind | Class | Hazard / Anchor | Mitigation |
|---|---|---|---|
| workspace.deleteDocument | A | — | none |
| workspace.deleteArtifact | A | — | none |
| workspace.deleteWire | A | — | none |
| workspace.moveFolder | A | — | none |
| document.batchRegister | A | — | none |
| document.liveProjection | A | — | none |
| block.update | A | — | none |
| block.delete | A | — | none |
| workspace.createWire | B1 | lazy `wireId` at `native-local-runtime.ts:1886` | drop fallback; require caller-supplied wireId |
| document.batchPrepare | B1 | lazy `batchId` at `native-local-runtime.ts:1566` | drop fallback; require caller-supplied batchId |
| workspace.createDocument | B1 + B2 | lazy `documentId` at `native-local-runtime.ts:1210`; `order ?? Date.now()` at `:1214` | drop UUID fallback; derive `order` from operation metadata |
| workspace.createFolder | B1 + B2 | lazy `folderId` at `native-local-runtime.ts:1276`; `order ?? Date.now()` at `:1280` | drop UUID fallback; derive `order` from operation metadata |
| import.webClip | B1 + B2 | lazy `documentId` at `native-local-runtime.ts:2191`; `order` drift via `writeWorkspaceDocument` | drop UUID fallback; same `order` fix |
| workspace.updateDocument | B2 | `updatedAt = Date.now()` at handler invocation | timestamp policy (see below) |
| workspace.updateFolder | B2 | `updatedAt = Date.now()` | timestamp policy |
| workspace.refreshWire | B2 | `snapshotAt = new Date().toISOString()` at `native-local-runtime.ts:1940` | timestamp policy |
| workspace.moveDocuments | B2 | `baseOrder ?? Date.now()` at `native-local-runtime.ts:1665`; `order` is load-bearing | derive `baseOrder` from operation metadata (higher severity — affects sort) |
| workspace.putArtifact | B2 | `updatedAt = now` at `native-local-runtime.ts:1779` | timestamp policy |
| workspace.deleteFolder | B2 | Y.Doc throw at `:1459` short-circuits filesystem cascade at `:1490-1495` on partial replay | gracefully short-circuit on missing folder, OR move filesystem cascade inside the transact, OR add janitor pass at startup |
| document.editComment | B2 | `updatedAt: now` at `native-local-runtime.ts:2582` | timestamp policy |
| crdt.flush | B2 | `revision` counter increments per call at `document_persistence_service.rs:167` | accept `expected_revision` from operation payload, or derive revision from Y.Doc state vector |
| document.write | B2 | same `revision`-counter drift via `save_document` | same as crdt.flush |
| block.editText | C | offset-based mutations on `Y.XmlText` at `block-mutations.ts:168-214`; replay applies stale offsets to mutated text → silent corruption | persistent op_id ledger in the document Y.Doc; check membership before applying mutations |
| block.insert | C | position-based `fragment.insert(index, elements)` at `block-mutations.ts:135` produces duplicate blocks even with explicit IDs on partial replay | pre-check `findBlockInFragment` for each block ID; skip if already present; op_id ledger as backstop |
| document.ingestMarkdownOriginal | C | unguarded `remove_file(&pending_path)` at `original_file_service.rs:141` fails on second attempt | replace with `remove_file_if_exists` semantics; require explicit documentId |
| document.uploadIngest | C | same `adoptPendingOriginalFile` consume-and-fail; missing `cleanup_pending_upload` finally | mirror `graph.importArchive` cleanup pattern; require explicit documentId |
| graph.importArchive | C | `create_graph_service` at `graph_service.rs:170-174` errors on existing `graph.json`; multi-step extraction with no atomic boundary | check graph existence + operation tag at handler entry; tag created entities with operationId for safe replay detection |
| import.vault | C | lazy UUIDs at `native-local-runtime.ts:2096`; multi-step entity creation; no source dedup | require entity IDs in payload or derive deterministically; tag with operationId; atomic-or-resumable extraction |

### Cross-cutting patterns

The audit surfaced three patterns that recur across multiple kinds. They are
worth naming so future contributors recognize the shape rather than
re-discovering it per-kind.

**The `order ?? Date.now()` pattern.** Five handlers fall back to wall-clock
for the `order` field when payload omits it: `writeWorkspaceDocument` at
`:1214` (used by createDocument, document.write, import.webClip),
`writeWorkspaceFolder` at `:1280` (createFolder), `moveWorkspaceDocuments` at
`:1665`. The `order` field is load-bearing for stable sort. Derive a single
helper `deterministicOrderFromOperation(operation)` that pulls from the
operation's journal-recorded enqueue timestamp; apply at all five sites.

**Throw-on-missing in delete handlers.** Three delete handlers throw if their
target is already gone: `deleteWire` at `:1956`, `deleteArtifact` at `:1798`,
`deleteFolder` at `:1459`. For deleteWire and deleteArtifact this is
functionally A — journal recovery filters completed ops, so the throw never
fires on a healthy replay. But for deleteFolder, the throw short-circuits a
downstream filesystem cascade, leaving orphaned documents. The pattern is
fragile across the cluster: a partial replay (transact succeeded, completion
didn't journal) produces a visible error that contradicts the final state.
Either short-circuit gracefully on missing target (return `deleted: false`) or
accept the error and document that journal recovery will report `Failed` even
when the final state is correct.

**Position-based vs. key-based mutations.** Y.Map operations keyed by stable
IDs (Y.Map.set, Y.Map.delete) are CRDT-idempotent. Operations on sequences by
position (`fragment.insert(index, ...)`, `Y.XmlText` offset deletions and
inserts) are not. They produce duplicates or corruption on replay even when
the caller supplies stable IDs, because the position-coordinate space changes
between attempts. block.insert and block.editText are both in this class. Any
new operation that mutates a Y.XmlFragment or Y.XmlText by position must
either pre-check existing state by ID or be guarded by a persistent op_id
ledger.

### `updatedAt` policy decision (pending)

Eight kinds carry `updatedAt = Date.now()` (or equivalent) drift on replay.
The taxonomy classifies this as B2. Two policy options:

- **Derive from operation metadata.** Compute `updatedAt` from the operation's
  journal enqueue timestamp at handler entry. Replay produces identical
  timestamps. Affects ~8 sites; needs a shared helper.
- **Accept the drift.** Document that `updatedAt` is non-monotonic across
  replay and ensure no downstream consumer relies on strict ordering. Sync
  layers and audit tooling must be reviewed.

The decision is upstream of any individual handler fix. It belongs in this
doc once made.

## Target State

The durable work substrate should eventually expose one mental model:

- Write intent: persisted before acceptance.
- Worker ownership: CRDT runtime, Rust worker, Python helper, or sidecar.
- Status: queued, running, succeeded, failed, cancelled, caller timed out.
- Projection boundary: accepted work is durable/live-authoritative; adapters
  that return projection-backed responses must either read from that authority or
  force a targeted projection flush.
- Progress: phase, message, counts, percent, updated timestamp, details.
- Recovery: startup finds non-terminal records and resumes or marks them failed
  with a reason.
- Idempotency: work carries stable operation/job IDs so replay is safe.
- Result: small results inline, large results spilled to files.
- Cancellation: cancellation is persisted and checked at worker checkpoints.

CRDT writes and large jobs do not need identical storage files, but they do need
the same acceptance semantics and status vocabulary.

## Why This Pairs Durable Writes With Async Large-Work

The old durable-write framing was too narrow. Journaling the CRDT queue prevents
one loss mode, but the real release requirement is broader:

- Headless MCP/local API writes must work when the UI is closed or busy.
- Large PDFs, archive imports, semantic indexing, and model setup must not freeze
  loopback or the Tauri UI.
- Request handlers must be able to return accepted job/write records quickly.
- Users and agents need progress, cancellation, retry, and failure inspection.
- Recovery must explain what happened after restart.

This is one substrate with two executors: CRDT authority for document/workspace
mutations and job workers for expensive non-CRDT work.

## Remaining Hardening

This slice starts the durable CRDT journal, recovery path, and large-work job
offload path, but several release blockers remain:

- Per-kind replay safety is documented in [Replay Strategy](#replay-strategy).
  20 of 28 kinds have a remaining mitigation requirement (6 non-convergent C,
  5 identity-hazard B1 including 3 dual-classified with B2, 8 metadata-drift
  B2, plus the deleteFolder cascade fix). These must be implemented before
  recovery can be called trusted.
- Operations polled by the frontend but never completed are recovered on restart,
  not immediately retried in-process.
- Completion journal append failure is logged but does not block queue
  completion; this avoids wedging the UI but leaves a narrow replay-risk window
  under disk failure.
- The worklog and job registry are still separate stores. A future consolidation
  should share record schemas, status APIs, and recovery policy.
- Some compatibility endpoints still have synchronous defaults, notably hosted
  REST artifact upload/import without `Prefer: respond-async`. These now have
  explicit read-after-write projection boundaries, but they still pay parser and
  materialization latency on the request path.
- Large parsers need deeper cancellation inside parser runtimes. Current jobs
  check cancellation at worker checkpoints, but cannot preempt every blocking
  third-party parser call.
