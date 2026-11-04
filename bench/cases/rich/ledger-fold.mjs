// rich.ledger.*: the W4 rich.emporium_log.* set ported id-for-id to the ledger (W7 section 6, bench-ao.md section 1): the same tiers and IT counts,
// but the writes go through the commit route (POST /emporium/commit/{graph}/{vocab}?wait=folded, so a write number includes the fold),
// the replays through the fold's replay route, and disk is the bytes of graphs/<id>/ledger/<vocab> under the gardend profile against the JSONL equivalent.
// The prototype's routes (/emporium/log/..., mode PUT, the Y.Array room) do not exist on this branch; only the measurement shape is ported.
//
// Dependencies: the e-cases seed single-record frames through the ledger room (lib/ledger.mjs, needs @noble/hashes). They do NOT need yjs / y-protocols
// (the W4 log room); _log.mjs still imports lib/ylog.mjs, which loads those lazily and tolerates their absence. The n-cases need no client library at all.
//
// Sizes. n counts RECORDS, one subject per record (4 set ops: title, weight, note, madeOn), tier-scaled as in rich.emporium_log (m: 10,000 / 100,000; s: 1,000 / 10,000).
// Seeding by commit route in requests of 500 records. e counts EVENTS: a frame per record, appended in bulk through the ledger room by a seeder origin (the W4 room push),
// since 100,000 acked requests would take hours. Frames from a foreign origin are folded like any other.
// Vocab 'workspace' with the Gizmo/Widget ontology proposed first, so the vocab has a contract (SHACL gate, write_target projection, emporium_query reads the sink).
// Predicate IRIs follow the type catalog: urn:sophia:type:<graph>:workspace#<field>; subjects are urn:gbench:ledger:<graph>:gizmo:<k> (see IRI_NOTE).
//
// Thresholds: SLICE-1.md section 6 with the section 11 correction (the ids' n are records; e-cases carry the events), plus e100k/e10k <= 1.25 and replay_live <= 60 s.
// The section 6 B/A ratios cannot be taken against A here (main has no commit route), so: write.b1.n10k is ratioed to write.b1.n0 on the same binary (marked in the note);
// query_eq and boot B/A stay `ab` (deferred to the A/B comparison, which skips them if A cannot run them).
import { IT, SUITES, graphFor, phasedRun, timedRun } from './_rich.mjs'
import { ONTOLOGY_CLASSES, gizmoRecords } from '../../fixture/parts/rich-gen.mjs'
import { sizeOf, eventsOf, tierOf, seedOf, Q } from './_log.mjs'
import * as ledger from '../../lib/ledger.mjs'
import * as cm from '../../lib/commit.mjs'

const VOCAB = 'workspace'
const WITNESS = 'urn:gbench:witness'
const WRITER = 'gbench'
const NO_FOLD = 'no ledger/fold on this binary (version route 404); nothing to measure'
const NO_DEPS = '@noble/hashes not installed (pnpm install in bench/)'
const IRI_NOTE = 'subject and predicate IRIs are the bench\'s reading of the type catalog (urn:sophia:type:<graph>:workspace#<field>); a 422 from the SHACL gate means the contract wants others'
const LSUITES = ['smoke', ...SUITES] // the cheap cases also run under `gbench run --suite smoke --cases rich.ledger --twin` (correctness)
const r2 = (x) => Math.round(x * 100) / 100
const rand = () => Math.random().toString(36).slice(2, 7)

const nsOf = (g) => `urn:sophia:type:${g}:${VOCAB}#`
const subjectOf = (g, k) => `urn:gbench:ledger:${g}:gizmo:${k}`
// One Gizmo record as four set ops on its own subject.
function opsOf(g, rec, k) {
  const ns = nsOf(g), s = subjectOf(g, k), cls = ns + 'Gizmo'
  return [cm.set(s, cls, ns + 'title', rec.title), cm.set(s, cls, ns + 'weight', rec.weight), cm.set(s, cls, ns + 'note', rec.note), cm.set(s, cls, ns + 'madeOn', cm.dateTime(rec.madeOn))]
}
const bodyOf = (ops) => ({ witness: WITNESS, writer: WRITER, ops })

// ---- graphs ----------------------------------------------------------------------------------------------------------------
async function open(ctx, name) {
  const g = await graphFor(ctx, name)
  await ctx.mcp('propose_domain_ontology', { graphId: g, vocab: VOCAB, classes: ONTOLOGY_CLASSES(), rationale: 'gbench' })
  ctx.noFold = (await ledger.version(ctx, g, VOCAB)) === null
  return g
}
const params = (ctx, unit) => { ctx.params.n_actual = ctx.L.n; ctx.params.n_unit = unit; ctx.params.tier = tierOf(ctx) }
const U_REC = 'records (one subject and one frame-op group each; seeded by commit requests of 500 records)'
const U_EV = 'events (single-record frames appended through the ledger room by a seeder origin)'

// A graph holding n records, written through the commit route, folded (settled) before it returns. Sets ctx.graphId, ctx.noFold, ctx.L.
async function recordGraph(ctx, name, n) {
  const g = await open(ctx, name), notes = []
  let seedMs = 0, jsonlBytes = 0
  if (ctx.noFold) notes.push(NO_FOLD)
  else if (n > 0) {
    const t0 = performance.now(), seed = seedOf(ctx)
    for (let s = 0; s < n; s += 500) {
      const recs = gizmoRecords(Math.min(500, n - s), seed, s), ops = []
      recs.forEach((rec, j) => { ops.push(...opsOf(g, rec, s + j)); jsonlBytes += Buffer.byteLength(JSON.stringify(rec) + '\n') })
      await cm.commit(ctx, g, VOCAB, bodyOf(ops))
    }
    const ackMs = Math.round(performance.now() - t0)
    const foldMs = Math.round(await cm.settle(ctx, g, VOCAB))
    seedMs = Math.round(performance.now() - t0); notes.push(`seeded ${n} records by commit requests of 500: acked in ${ackMs} ms, fold caught up ${foldMs} ms later`)
  }
  ctx.L = { n, nUnit: 'records', graphId: g, noFold: ctx.noFold, seedMs, jsonlBytes, notes }
  ctx.log({ ledgerGraph: g, n, seedMs, noFold: ctx.noFold })
  return ctx.L
}
// A graph holding nEvents single-record frames appended through the ledger room, folded before it returns. ctx.L.n counts EVENTS.
async function eventGraph(ctx, name, nEvents) {
  const g = await open(ctx, name), notes = []
  let seedMs = 0, jsonlBytes = 0
  if (ctx.noFold) notes.push(NO_FOLD)
  else if (!ledger.available) throw new Error(NO_DEPS)
  else {
    const t0 = performance.now(), seed = seedOf(ctx), cls = nsOf(g) + 'Gizmo', origin = `seed-${rand()}`
    let cs = -1, chunk = []
    const recAt = (k) => { const c0 = k - (k % 1000); if (c0 !== cs) { chunk = gizmoRecords(1000, seed, 2_000_000 + c0); cs = c0 } return chunk[k - c0] }
    const S = ledger.connect(ctx.url, ctx.key, g, VOCAB, origin)
    try {
      await S.synced()
      await ledger.appendMany(S, nEvents, (j) => {
        const k = `e${j}`, payload = JSON.stringify({ ops: opsOf(g, recAt(j), k) })
        jsonlBytes += Buffer.byteLength(payload) + 1
        return { witness: WITNESS, subjects: [{ subject: subjectOf(g, k), class: cls, parents: [] }], mode: 'assert', payload }
      }, { chunk: 2000 })
      await ledger.waitVersion(ctx, g, VOCAB, (v) => (v.durable?.[origin] ?? 0) >= nEvents, { timeoutMs: 600_000 })
      if (S.errors.length) throw new Error(`seeder client errors: ${S.errors.slice(0, 2).join('; ')}`)
    } finally { S.close() }
    const pushMs = Math.round(performance.now() - t0), foldMs = Math.round(await cm.settle(ctx, g, VOCAB))
    seedMs = Math.round(performance.now() - t0); notes.push(`seeded ${nEvents} single-record frames by room push (origin ${origin}): durable in ${pushMs} ms, fold caught up ${foldMs} ms later`)
  }
  ctx.L = { n: nEvents, nUnit: 'events', graphId: g, noFold: ctx.noFold, seedMs, jsonlBytes, notes }
  ctx.log({ eventGraph: g, nEvents, seedMs, noFold: ctx.noFold })
  return ctx.L
}
// Built once per run and key, and reused (ctx.shared is per family): the replay and disk cases share one graph per size.
async function shared(ctx, key, build) {
  const cache = (ctx.shared.ledgerFold ??= {})
  if (cache[key]) { const L = cache[key]; ctx.graphId = L.graphId; ctx.noFold = L.noFold; ctx.L = L; return L }
  return (cache[key] = await build())
}

const meta = (ctx, extra = {}) => ({ extra: { tier: tierOf(ctx), n: ctx.L?.n, nUnit: ctx.L?.nUnit, seedMs: ctx.L?.seedMs, noFold: !!ctx.noFold, notes: [`actual n ${ctx.L?.n} ${ctx.L?.nUnit} (tier ${tierOf(ctx)})`, ...(ctx.L?.notes ?? [])], ...extra } })
const mk = (id, it, extra) => ({ id, version: 1, kind: 'rich', suites: SUITES, params: IT(...it), needs: [], ...extra })
const sizeFor = (ctx, s) => (s === 'n0' ? 0 : sizeOf(ctx, s === 'n100k' ? 'large' : 'small'))
const evFor = (ctx, s) => eventsOf(ctx, s === 'e100k' ? 'large' : 'small')

// ---- thresholds (SLICE-1.md section 6, section 11 correction) -----------------------------------------------------------------
const T = {
  b1n10k: { name: 'write.b1.n10k / write.b1.n0 (p50)', metric: 'p50', ratioTo: 'rich.ledger.write.b1.n0', op: '<=', limit: 1.2, note: 'section 6 says B/A <= 1.20; A has no commit route, so the control is the empty ledger on the same binary' },
  b1n100k: { name: 'write.b1.n100k / write.b1.n10k (p50)', metric: 'p50', ratioTo: 'rich.ledger.write.b1.n10k', op: '<=', limit: 1.25, note: 'section 6: the append must not scale with the log' },
  b1e100k: { name: 'write.b1.e100k / write.b1.e10k (p50)', metric: 'p50', ratioTo: 'rich.ledger.write.b1.e10k', op: '<=', limit: 1.25, note: 'section 12 row: the append must not scale with the log, measured in events' },
  queryEq: { name: 'query_eq B/A same', metric: 'p50', ab: true, op: '<=', limit: 1, note: 'the projection is the same store; judged by the A/B comparison (`same`)' },
  oracleEqual: { name: 'oracle equal on every round', metric: 'extra.equal', op: '==', limit: true, note: 'the fold is the disposability oracle' },
  oracleNoErrors: { name: 'oracle iterations without error', metric: 'errors', op: '==', limit: 0 },
  replayLive: { name: 'live replay p50', metric: 'p50', op: '<=', limit: 60_000, unit: 'ms', note: 'a rebuild is an admin act, not a boot cost' },
  disk: { name: 'ledger bytes / JSONL bytes', metric: 'extra.ratio', op: '<=', limit: 3, note: 'frame overhead stays bounded' },
  boot: { name: 'boot.first_query B/A', metric: 'p50', ab: true, op: '<=', limit: 1.1, note: 'the ledger does not load or fold at boot; judged by the A/B comparison' },
}

// ---- case bodies -------------------------------------------------------------------------------------------------------------
// One timed write per iteration; the request body is built untimed (prep), the commit with ?wait=folded is timed, so the number includes the fold.
async function writeRun(ctx, recordsPerRequest) {
  if (ctx.noFold) return { skipped: NO_FOLD }
  const g = ctx.graphId, base = 1_000_000, seed = seedOf(ctx)
  const out = await phasedRun(ctx, ctx.params, async (c, i) => {
    const start = base + i * recordsPerRequest, ops = []
    gizmoRecords(recordsPerRequest, seed, start).forEach((rec, j) => ops.push(...opsOf(g, rec, start + j)))
    c._body = bodyOf(ops)
  }, async (c) => { await cm.commit(c, g, VOCAB, c._body, { wait: true }) })
  return { calls: out.calls, ...meta(ctx, { recordsPerRequest, waitFolded: true, notes: [`actual n ${ctx.L.n} ${ctx.L.nUnit} (tier ${tierOf(ctx)})`, ...ctx.L.notes, `timed: one commit of ${recordsPerRequest} record(s) with ?wait=folded; the body is built untimed`, IRI_NOTE] }) }
}
async function replayRun(ctx, into, { timeoutMs = 900_000, requireEqual = true } = {}) {
  if (ctx.noFold) return { skipped: NO_FOLD }
  const rows = []
  const out = await timedRun(ctx, ctx.params, async () => {
    const r = await cm.replay(ctx, ctx.graphId, VOCAB, into, timeoutMs), j = r.json
    rows.push({ ms: j.ms ?? r.ms, clientMs: r.ms, events: j.events, triples: j.triples, equal: j.equal, missing: j.missing, extra: j.extra })
    if (requireEqual ? j.equal !== true : j.equal === false) throw new Error(`${into} replay not equal: missing ${j.missing} extra ${j.extra}`)
  })
  const m = meta(ctx, { replayMs: rows.map((r) => r.ms), clientMs: rows.map((r) => Math.round(r.clientMs)), events: rows.at(-1)?.events, triples: rows.at(-1)?.triples, equal: rows.length > 0 && rows.every((r) => requireEqual ? r.equal === true : r.equal !== false) })
  m.extra.notes.push(`server replay ms: ${rows.map((r) => r.ms).join(', ')}; events ${rows.at(-1)?.events}, triples ${rows.at(-1)?.triples}`)
  return { calls: out.calls, ...m }
}
// Reported, not timed: ledger/ bytes on disk (once they stop growing) against the same data as JSONL, summed client-side.
async function diskRun(ctx) {
  if (ctx.noFold) return { skipped: NO_FOLD }
  if (!ctx.profile) return { skipped: 'disk needs a spawned profile (not --attach)' }
  let bytes = null
  const out = await timedRun(ctx, ctx.params, async () => { bytes = await cm.settledLedgerBytes(ctx.profile, ctx.graphId, VOCAB); if (bytes === null) throw new Error(`no ledger directory at ${cm.ledgerDir(ctx.profile, ctx.graphId, VOCAB)}`) })
  const jsonl = ctx.L.jsonlBytes, ratio = jsonl && bytes ? r2(bytes / jsonl) : null, st = await cm.foldStatus(ctx, ctx.graphId, VOCAB)
  const m = meta(ctx, { ledgerBytes: bytes, jsonlBytes: jsonl, ratio, version: st?.version })
  m.extra.notes.push(`ledger bytes ${bytes}, JSONL ${jsonl} (${ctx.L.nUnit === 'events' ? 'the frames\' payload lines' : 'the records'}), ratio ${ratio} (threshold <= 3)`)
  return { calls: out.calls, ...m }
}
// Process start to the first emporium_query eq ok on a graph with the large ledger; also when the fold had caught up (lag_frames 0), reported as extra.foldCaughtUpMs.
async function bootRun(ctx) {
  const calls = [], reps = ctx.params.iterations, healthy = [], caught = []
  for (let rep = 0; rep < reps; rep++) {
    const g = await ctx.proc.start(), t0 = g.t0
    healthy.push(Math.round(await g.waitHealthy()))
    const c = g.client(); let ok = true, error = null, traceId = null
    const r = await c.call('emporium_query', { graphId: ctx.graphId, vocab: VOCAB, class: 'Gizmo', criteria: { weight: { eq: 37 } }, limit: 25 })
    ok = r.ok; traceId = r.traceId; if (!r.ok) error = String(r.error).slice(0, 200)
    calls.push({ traceId, ms: performance.now() - t0, ok, error })
    try { await cm.settle({ url: g.url, key: c.key }, ctx.graphId, VOCAB, { timeoutMs: 300_000 }); caught.push(Math.round(performance.now() - t0)) } catch (e) { caught.push(`error: ${String(e.message).slice(0, 80)}`) }
    ctx.log({ boot_first_query: rep + 1, ms: Math.round(calls.at(-1).ms), ok, foldCaughtUpMs: caught.at(-1) })
    if (rep < reps - 1) await ctx.proc.stop()
  }
  const o = meta(ctx, { healthyMs: healthy, foldCaughtUpMs: caught }); o.extra.notes.push('process start to first emporium_query eq ok; foldCaughtUpMs = process start to lag_frames 0')
  return { calls, ...o }
}

export const cases = [
  // ---- writes: one record (a cell edit) onto a graph whose ledger already holds n records / events; a fresh graph per case
  ...['n0', 'n10k', 'n100k'].map((s) => mk(`rich.ledger.write.b1.${s}`, [[3, 1], [30, 3]], {
    suites: s === 'n100k' ? SUITES : LSUITES, thresholds: s === 'n10k' ? [T.b1n10k] : s === 'n100k' ? [T.b1n100k] : undefined,
    async setup(ctx) { await recordGraph(ctx, `fw1${s}`, sizeFor(ctx, s)); params(ctx, U_REC) },
    async run(ctx) { return writeRun(ctx, 1) } })),
  // A 100-record batch (a CSV paste), one request, one frame.
  ...['n0', 'n10k'].map((s) => mk(`rich.ledger.write.b100.${s}`, [[2, 1], [8, 2]], {
    suites: s === 'n0' ? LSUITES : SUITES,
    async setup(ctx) { await recordGraph(ctx, `fw100${s}`, sizeFor(ctx, s)); params(ctx, U_REC) },
    async run(ctx) { return writeRun(ctx, 100) } })),
  // emporium_query eq on weight after the ledger is folded: the projection should cost the same as on main.
  mk('rich.ledger.query_eq.n10k', [[5, 1], [30, 3]], {
    suites: LSUITES, thresholds: [T.queryEq],
    async setup(ctx) { await shared(ctx, `r${sizeFor(ctx, 'n10k')}`, () => recordGraph(ctx, 'fqe10', sizeFor(ctx, 'n10k'))); params(ctx, U_REC) },
    async run(ctx) {
      if (ctx.noFold) return { skipped: NO_FOLD }
      const out = await timedRun(ctx, ctx.params, (c, i) => Q(c, { weight: { eq: (i * 37) % 1000 } }))
      return { calls: out.calls, ...meta(ctx) }
    } }),
  // The oracle: rebuild the fold in memory into a scratch store and compare with the live sink. equal must be true, else the iteration errors.
  ...['n10k', 'n100k'].map((s) => mk(`rich.ledger.replay_oracle.${s}`, [[1, 0], [3, 1]], {
    suites: s === 'n10k' ? LSUITES : SUITES, thresholds: [T.oracleEqual, T.oracleNoErrors],
    async setup(ctx) { await shared(ctx, `r${sizeFor(ctx, s)}`, () => recordGraph(ctx, s === 'n10k' ? 'fro10' : 'fro100', sizeFor(ctx, s))); params(ctx, U_REC) },
    async run(ctx) { return replayRun(ctx, 'oracle') } })),
  // Rebuild the live sink from nothing (an admin act). Leaves the shared graph equal again.
  mk('rich.ledger.replay_live.n100k', [[1, 0], [2, 1]], {
    thresholds: [T.replayLive],
    async setup(ctx) { await shared(ctx, `r${sizeFor(ctx, 'n100k')}`, () => recordGraph(ctx, 'fro100', sizeFor(ctx, 'n100k'))); params(ctx, U_REC) },
    async run(ctx) { return replayRun(ctx, 'live', { requireEqual: false }) } }),
  // Reported, not timed: ledger bytes against the same records as JSONL.
  mk('rich.ledger.disk.n100k', [[1, 0], [1, 0]], {
    thresholds: [T.disk],
    async setup(ctx) { await shared(ctx, `r${sizeFor(ctx, 'n100k')}`, () => recordGraph(ctx, 'fro100', sizeFor(ctx, 'n100k'))); params(ctx, U_REC) },
    async run(ctx) { return diskRun(ctx) } }),
  // Process start to the first query_eq ok on a graph with the large ledger. Restarts on the same profile; the last rep stays up.
  mk('rich.ledger.boot.first_query.n100k', [[1, 0], [3, 0]], {
    kind: 'boot', needs: ['proc'], thresholds: [T.boot],
    async setup(ctx) {
      const g = await ctx.proc.start(); await g.waitHealthy()
      await recordGraph(ctx, 'fbt100', sizeFor(ctx, 'n100k')); params(ctx, U_REC)
      await ctx.proc.stop() // flush; every rep below is a cold start
    },
    async run(ctx) { return ctx.noFold ? { skipped: NO_FOLD } : bootRun(ctx) } }),

  // ---- event scale: the ledger is seeded with single-record frames through the room; the ids' e10k / e100k are nominal (tier m: 10,000 / 100,000; tier s: 1,000 / 10,000)
  ...['e10k', 'e100k'].map((s) => mk(`rich.ledger.write.b1.${s}`, [[3, 1], [30, 3]], {
    suites: s === 'e10k' ? LSUITES : SUITES, thresholds: s === 'e100k' ? [T.b1e100k] : undefined,
    async setup(ctx) { await eventGraph(ctx, `fw1${s}`, evFor(ctx, s)); params(ctx, U_EV) },
    async run(ctx) { return writeRun(ctx, 1) } })),
  // Rebuild the live sink from 100k events (an admin act). Runs first on the shared e100k graph; 900 s client timeout.
  mk('rich.ledger.replay_live.e100k', [[1, 0], [1, 0]], {
    thresholds: [T.replayLive],
    async setup(ctx) { await shared(ctx, `e${evFor(ctx, 'e100k')}`, () => eventGraph(ctx, 'fle100', evFor(ctx, 'e100k'))); params(ctx, U_EV) },
    async run(ctx) { return replayRun(ctx, 'live', { requireEqual: false }) } }),
  // The oracle on the same shared e100k graph, after the live rebuild.
  mk('rich.ledger.replay_oracle.e100k', [[2, 0], [2, 0]], {
    thresholds: [T.oracleEqual, T.oracleNoErrors],
    async setup(ctx) { await shared(ctx, `e${evFor(ctx, 'e100k')}`, () => eventGraph(ctx, 'fle100', evFor(ctx, 'e100k'))); params(ctx, U_EV) },
    async run(ctx) { return replayRun(ctx, 'oracle') } }),
  // Reported, not timed: ledger bytes against the seeded frames' payloads as JSONL (one line per event).
  mk('rich.ledger.disk.e100k', [[1, 0], [1, 0]], {
    thresholds: [T.disk],
    async setup(ctx) { await shared(ctx, `e${evFor(ctx, 'e100k')}`, () => eventGraph(ctx, 'fle100', evFor(ctx, 'e100k'))); params(ctx, U_EV) },
    async run(ctx) { return diskRun(ctx) } }),
  // Cold start to the first query_eq on a graph holding 100k frames; builds its own graph (restarts the cell, so the shared one is not used).
  mk('rich.ledger.boot.first_query.e100k', [[1, 0], [3, 0]], {
    kind: 'boot', needs: ['proc'], thresholds: [T.boot],
    async setup(ctx) {
      const g = await ctx.proc.start(); await g.waitHealthy()
      await eventGraph(ctx, 'fbte100', evFor(ctx, 'e100k')); params(ctx, U_EV)
      await ctx.proc.stop()
    },
    async run(ctx) { return ctx.noFold ? { skipped: NO_FOLD } : bootRun(ctx) } }),
]
