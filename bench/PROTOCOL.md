# gbench protocol v1

gbench measures gardend through its own API (MCP over HTTP, plus the y-websocket rooms) on a synthetic corpus, one case at
a time, and joins each call to the server's OpenTelemetry spans by trace id. A number without the run id and file that
produced it is not a claim.

## How to run

- `env` prints the fingerprint (cpu, load, power, disk, and every gardend this run did not start).
- `fixture --tier s|m|l --seed N` seeds a deterministic corpus (seeded PRNG, an invented syllable lexicon, no real text)
  through gardend's API and caches the stopped profile in `~/.cache/sophia-bench/fixtures/<tier>-<seed>-p<protocol>-<gitRev>[-<partsHash>]/`.
  Tier s: 40 notes, a 300-block book. Tier m: 400 notes (about 30 blocks), a 2,500-block book, 200 comments, 300 wires, 12 folders.
  Seeding time is recorded in `run.json` `fixture.seedMs` (itself a bulk-create scaling measurement).
- `run --suite smoke|standard --tier t --label l --out dir` (spawn mode): APFS-clones the fixture (`cp -c -R`), starts gardend on a free
  port at 7790 or above with `GARDEN_OTEL_FILE`, runs the cases, stops **only the PID it started** with SIGINT (the final flush
  can take about 20 s), joins spans, writes `run.json`, `summary.md`, `spans.jsonl.gz`, and removes the clone.
  `smoke` is for checking the instrument (tier s, about 3 minutes). `standard` is for numbers (tier m).
- `run --attach url --key-file f`: measures a running instance in a graph `bench` (created if absent), inside a run folder of its own;
  nothing is deleted; no boot cases and no server spans.
- `compare A B`, `ab --a binA --b binB --rounds R`, `publish runDir --url --key-file [--baseline]` (RDF into graph `perf`, plus a summary document).

## Noise rules

1. Quiet machine, AC power, `uptime` load1 under about 6 (the run records load before and after; a busy start is noted in `notes`).
2. Measured runs (baselines, `ab`, anything that will be compared or published) take the machine lock (`--measured`, automatic in `ab`):
   `~/.cache/sophia-bench/locks/measure/` by atomic mkdir; they wait up to 10 minutes for `locks/dev/` to drain and record `env.contention`.
   Dev work registers in `locks/dev/` and yields to a measured run. Do not run builds beside a measured run.
3. Foreign gardends (for example the one on :5312) are recorded in `env.otherGardends` and never touched.
4. Compare **interleaved**: `ab` runs A B A B in turn on the same fixture snapshot, so drift hits both sides alike. Compare the pooled samples.
5. Judge by the bootstrap CI of the ratio of medians, never by two single numbers. `same` means the CI contains 1.0 or |ratio-1| < 0.05.
6. Look at the noise floor first: run the same binary twice and `compare`. The spread of those ratios is what a real change must beat.
7. **The machine is part of the measurement** (`lib/power.mjs`, since 2026-10-26). Every run, or the whole `ab` sitting, holds
   `caffeinate -i -m -s -w <pid>`. It samples everyone else's CPU every 5 s and, after each round (once our gardend has
   stopped), reads `pmset -g log` for the round's window: sleeps, DarkWake, full wakes and thermal events. It also records
   the power source and CPU speed limit at start and end. All of this goes in `env.machine`.

   A round is **clean** only with no sleep, no DarkWake, no thermal event, on AC, and unthrottled.

   `compare` and `ab` print **NOT A JUDGE** and exit 3 unless every round on both sides is clean, and `ab` prints the median
   ratio of each interleaved pair. A pooled verdict that the pairs disagree on is drift, not a result.

   Why: wave 1's first sitting (10-25 night) ran entirely on a Mac in Deep Idle. Its "70 s queue stall" was a 74 s
   Maintenance Sleep in the middle of a request, and its "4× drift" was DarkWake. A gardend span whose wall time far exceeds
   `busy_ns + idle_ns` was slept through (Rust's `Instant` stops in sleep). `gbench audit <dir>` reconstructs the machine
   state for older runs.

## Cases (protocol v1)

Case ids are `<kind>.<name>`. `caseVersion` must be bumped whenever a case's meaning changes; `compare` skips pairs whose versions differ.

- **read.** `read_document.small`, `.book_page` (a 64 KB page of the 2,500-block book), `read_blocks.book` (100 blocks), `list_documents`, `search_documents`,
  `sparql.count` (a full scan count), `sparql.titles`, `sparql.wires_2hop`, `get_backlinks`, `get_workspace`, `context_bundle`, `document_digest.book`.
  Why: the read path of every screen; the SPARQL ones expose the projection store.
- **write.** `insert_blocks.small|book`, `edit_block_text.small|book` (one keystroke-size edit: the cost of an edit on a small vs a 2,500-block document),
  `edit_comment.small`, `create_document`, `write_document.medium` (30 blocks), `create_wires`, `create_folder`, `create_restore_point`.
  Why: each is an acked durable write; the fsync count per op and lease hold time come from the spans.
- **burst.** `insert_blocks.same_doc_x8`, `insert_blocks.distinct_docs_x8` (8 concurrent callers; a sample is one call, `extra.roundWallMs` the rounds),
  `read_under_write` (read_document.small while 8 writers loop; the writers are not samples). Why: lease and queue contention.
- **scale.** `create_documents`: create documents 1..N in a fresh graph. `slope.msPerHundredDocs` is a least-squares fit; O(N) per op shows as a positive slope.
- **boot.** `cold_start` (process spawn to `/health` 200), `first_read` (spawn to the first read_document of the book), `first_sparql` (spawn to the first count; it runs after
  first_read, so it is cumulative). Three restarts on the same profile; `extra.callMs` has the call alone.
- **ws.** `open.book` (new WebSocket to the first server frame), `typing.book` (client A types at 20 keystrokes per second into the book's first text; the sample is the time until client B holds
  the update; a backlog shows as latencies that climb). Needs `pnpm install` in `bench/` (yjs). Not joined to spans (the socket carries no traceparent).

## Reading a result

`summary.md` has the headline table (n, errors, p50, p95, max, fsyncs per op, lease hold p95, queue wait p95), the five slowest cases with
their top self-time spans (milliseconds per op) and the slowest call's span tree, the span join fraction, and the unjoined (background) spans by total time.

- `server.callsJoined/callsTotal`: how many calls found their trace. Work that leaves the request (the debounced projection, the document save, lease lifecycle) is **unjoined** unless it carries the context; it is listed in `spanJoin.unjoinedTop` and is often the larger part of the cost of a write. Read it beside the case table.
- `crdt.op.apply` spans of recovered operations are excluded from `queueWaitMs`.
- Self time = span duration minus the union of its children.

## Adding a case

Create `bench/cases/<family>/<name>.mjs` exporting `cases` (SCHEMA §5): `{id, version, kind, suites, params, needs, setup?, op(ctx,i,k), teardown?}`; optional `run(ctx)`
(custom measurement returning `{calls:[{ms,ok,traceId}], extra}`), `during(ctx, signal)` (background load), `finish(ctx, samples)` (extras and slope). The runner owns timing, warmup, traceparent, join and stats.
`ctx.mcp(tool, args)` throws on error; `ctx.fixture` carries ids; `ctx.rng()` is seeded. Fixture parts live in `bench/fixture/parts/*.mjs` (`export const name; export async function seed(ctx)`; the returned ids merge into `ctx.fixture`).
Never put document text, SPARQL text or prompts in spans or in the ledger. `GBENCH_NO_PARTS=1` ignores fixture parts.
