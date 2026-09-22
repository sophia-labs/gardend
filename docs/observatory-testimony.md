# Gardend Observatory testimony

This is the deployment and verification contract for lifecycle testimony from
the **headless `gardend` process only**. It does not add a desktop/Tauri logging
path, a collector client, or an application-wide logging facade. Gardend owns
the cell-local facts it can directly witness: hydrate/setup, ready, periodic or
final durable flush, drain/quiescence, and terminal state.

## Enablement and identity

Capture is disabled unless this exact cross-service flag is true:

```text
SOPHIA_OBSERVATORY_CAPTURE_ENABLED=true
SOPHIA_OBSERVATORY_CONTRACT_BUNDLE_SHA256=749ef231fdcfea31fbc0ad9dee6647897eb1581f18dd36b9cf37188a2cbdca68
```

Unset, empty, `false`, and `0` are disabled. `true` and `1` are enabled
(case-insensitive for the words). Any other value is a startup configuration
error. Disabled mode creates no writer thread, emits no CaptureEvent lines, and
does not require cloud identity. This preserves standalone `gardend`,
`sophia-mcp --backend local`, and desktop behavior.

When capture is enabled, Gardend first requires the exact ratified CaptureEvent
v0.1 bundle hash above. A missing or different value is a startup error before
any testimony is emitted. The gateway must also inject all three identity
values into the spawned cell pod:

```text
GARDEN_CELL_GRAPH_ID=<validated public graph id>
GARDEN_CELL_ID=<opaque physical cell id> # owner-scoped cells only
GARDEN_CELL_MACHINE_ID=cell:<same telemetry graph id>
GARDEN_CELL_MACHINE_RUN_ID=<gateway-minted 26-character ULID>
```

For legacy cells, the telemetry graph id is `GARDEN_CELL_GRAPH_ID`. For an
owner-scoped cell carrying `GARDEN_CELL_ID`, the opaque cell id is the telemetry
graph id and `GARDEN_CELL_GRAPH_ID` remains the public owner-local graph id used
by Garden's graph boundary. This keeps gateway spawn/reap and Gardend lifecycle
testimony on one stable physical identity without conflating it with the public
route name.

Gardend fails startup with exit code 5 if the bundle pin or an identity value is
absent, malformed, or inconsistent. It never derives a graph from a local path,
repairs a mismatched Machine ID, or mints its own cloud MachineRun ID. The
resulting witness is `cell:<telemetry_graph_id>/<machine_run_id>`.

Each pod incarnation gets a new run ID. The telemetry graph ID and Machine ID
remain stable across replacement; the run ID must not. The bundle hash and identity
variables are not secrets. Disabled mode ignores them and does not require
either cloud identity or a contract pin.

## Process I/O and bounds

With capture enabled, fd 1 is exclusively canonical one-line CaptureEvent
NDJSON. Ordinary diagnostics, panics, and configuration errors remain on fd 2.
The writer has one dedicated thread and a bounded non-blocking queue; product
paths never perform collector/network I/O and drop newest when the queue is
full. Shutdown waits at most 250 ms for already accepted lines.

Writer tuning exists for testing and exceptional operations:

| Variable | Default | Meaning |
|---|---:|---|
| `GARDEN_CAPTURE_QUEUE_CAPACITY` | `256` | Bounded pending-line capacity; non-positive/invalid values use the default. |
| `GARDEN_CAPTURE_HEARTBEAT_SECONDS` | `60` | Timer-driven loss/health testimony interval; non-positive/invalid values use the default. |

Every line is at most 4096 bytes. Unsafe IDs, timestamps, sequence numbers,
durations, or payload integers outside JavaScript's exact integer range are
dropped before serialization and consume a sequence number.

Kubernetes' `kubectl logs` presentation combines stdout and stderr. The log
pipeline must retain the CRI `stream` field and apply the positive schema gate
only to `stream=stdout`; `stream=stderr` stays ordinary diagnostic logging.

## Contract pin

The centrally owned platform-next contract is ratified as CaptureEvent v0.1.
Its authoritative bundle/conformance SHA-256 is
`749ef231fdcfea31fbc0ad9dee6647897eb1581f18dd36b9cf37188a2cbdca68`.
That identity covers the golden source, JSON schema, valid corpus, and invalid
corpus. The golden-source-only SHA-256,
`7b266e107fd28d2370a49bee8d718838f121a0c102a9d584e067c47f0e11c390`,
is retained as a diagnostic but is not the runtime compatibility pin.

Garden's byte fixture is the exact nine-line cell-lifecycle excerpt of the
ratified valid corpus:
`src-tauri/observatory-fixtures/cell-v0.1-ea423e43.ndjson` (SHA-256
`f41ff96a1aaf35253848d9c8b8969acab2e8c46040c7ef89ecef8878232f8ccd`).
It proves serializer byte order and remains a reviewed excerpt, not a second
contract authority. Any future contract revision must update the baked runtime
pin, bundle-qualified fixture, serializer tests, browser metadata, and
deployment value together.

Do not edit or fork the contract source from Garden.

## Local proof

From the Garden repository root, with the frozen platform-next checkout at
`../pn-machine-lifecycle` (or set `OBSERVATORY_PLATFORM_NEXT_DIR`):

```sh
cargo test --manifest-path src-tauri/Cargo.toml --locked \
  --no-default-features --features headless --lib
cargo check --manifest-path src-tauri/Cargo.toml --locked \
  --no-default-features --features headless --example gardend
pnpm --dir frontend test:gardend-lifecycle-browser
```

The browser harness proves, with a real Gardend process and Chromium/Yjs:

- default-off startup works without cell identity and emits zero stdout bytes;
- enabled startup with a missing or mismatched bundle pin fails closed on
  stderr and emits no stdout;
- enabled startup with the correct pin but missing identity also fails closed;
- an open browser WebSocket pins a cell beyond its idle TTL;
- a browser-originated Yjs update is included in the final durable flush;
- a fresh process restores that update and testifies `boot_mode=restored`;
- both enabled incarnations pass the platform positive gate; and
- boot → drain → quiesce → final flush → terminal order is preserved.

Artifacts land under `frontend/test-results/gardend-lifecycle/` by default.
Set `GARDEND_BROWSER_HEADED=1` to watch the Chromium portion.

## Linux image candidate

The cloud image is built from the Garden repository root and the committed
`Cargo.lock`; no platform source is copied into it:

```sh
docker build --platform linux/amd64 \
  --build-arg CARGO_JOBS=2 \
  -f Dockerfile.gardend \
  -t gardend:observatory-candidate .
```

This is a local proof command only. Do not tag or push to ECR from this branch.
The image must start normally with capture absent/false. A canary deployment
must add the enablement flag and exact bundle pin while the gateway continues
to supply the exact identity environment above.

## Canary smoke probes

After a separately approved image push and Helm diff:

1. Confirm the gateway's rendered spawned-pod environment contains the flag,
   exact v0.1 bundle hash, and all three identity values, and that the Machine
   ID is `cell:<graph>`.
2. Open a disposable graph through the gateway, keep its Yjs WebSocket open
   past the configured idle TTL, and confirm the pod remains Ready.
3. Write a sentinel Yjs value, close the socket, and wait for idle shutdown.
4. In the CRI/collector stream, positive-gate only stdout and confirm one
   terminal-success lifecycle for the gateway-minted run ID. Confirm ordinary
   Gardend logs are present only as stderr records.
5. Reopen the graph, confirm a new MachineRun ID, `boot_mode=restored`, and the
   sentinel value. Close it and confirm a second ordered terminal lifecycle.
6. Query the ledger by both run IDs and compare accepted event counts with the
   raw stdout records before considering capture live.

Do not infer a healthy testimony path merely from a Ready pod: the stdout gate,
stream separation, restored boot, final flush, and ledger query are independent
smoke conditions.
