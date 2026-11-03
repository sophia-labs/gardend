// rich.emporium_log.*: the Emporium event log (a Y.Array of write events per graph+vocab) against today's write path.
// On a binary without the log (mode PUT 404) every case still runs and measures today's path; replay/disk cases skip with a note.
import { IT, SUITES, timedRun } from './_rich.mjs'
import { gizmoRecords } from '../../fixture/parts/rich-gen.mjs'
import { logGraph, sharedLogGraph, eventGraph, sharedEventGraph, eventsOf, sizeOf, seedOf, W, Q, logStats, replay, settledStats, NO_LOG, tierOf } from './_log.mjs'

const mk = (id, it, extra) => ({ id, version: 1, kind: 'rich', suites: SUITES, params: IT(...it), needs: [], ...extra })
const r1 = (x) => Math.round(x * 10) / 10
const sizeKey = (s) => (s === 'n100k' ? 'large' : s === 'n10k' ? 'small' : null)

// Note carried in every result: the actual n and the tier (the id's n10k / n100k are nominal).
const meta = (ctx, extra = {}) => ({ extra: { tier: tierOf(ctx), n: ctx.L?.n, seedMs: ctx.L?.seedMs, noLog: !!ctx.noLog, notes: [`actual n ${ctx.L?.n} (tier ${tierOf(ctx)})`, ...(ctx.L?.notes ?? [])], ...extra } })
const evFor = (ctx, s) => eventsOf(ctx, s === 'e100k' ? 'large' : 'small')
const evParams = (ctx) => { ctx.params.n_actual = ctx.L.n; ctx.params.n_unit = 'events (single-record, pushed into the log room)'; ctx.params.tier = tierOf(ctx) }
const sizeFor = (ctx, s) => (s === 'n0' ? 0 : sizeOf(ctx, sizeKey(s)))

// Skipping facility: a case that cannot run returns n = 0 via params and a note instead of failing.
const skipRun = (ctx) => { ctx.params.iterations = 0; ctx.params.warmup = 0; return { calls: [], extra: { skippedReason: NO_LOG, notes: [NO_LOG, ...(ctx.L?.notes ?? [])] } } }

// A timed run that carries meta, so params get the actual n.
async function run(ctx, fn) {
  const out = await timedRun(ctx, ctx.params, fn)
  ctx.params.n_actual = ctx.L?.n; ctx.params.n_unit = 'records (events = ceil(n/100))'; ctx.params.tier = tierOf(ctx)
  return { calls: out.calls, ...meta(ctx, out.extra ? { ...out.extra } : {}) }
}

async function bootRun(ctx) {
  const calls = [], reps = ctx.params.iterations, healthy = []
  for (let rep = 0; rep < reps; rep++) {
    const g = await ctx.proc.start(), t0 = g.t0
    healthy.push(Math.round(await g.waitHealthy()))
    const c = g.client(); let ok = true, error = null, traceId = null
    const r = await c.call('emporium_query', { graphId: ctx.graphId, vocab: 'workspace', class: 'Gizmo', criteria: { weight: { eq: 37 } }, limit: 25 })
    ok = r.ok; traceId = r.traceId; if (!r.ok) error = String(r.error).slice(0, 200)
    calls.push({ traceId, ms: performance.now() - t0, ok, error })
    ctx.log({ boot_first_query: rep + 1, ms: Math.round(calls.at(-1).ms), ok })
    if (rep < reps - 1) await ctx.proc.stop()
  }
  const o = meta(ctx, { healthyMs: healthy }); o.extra.notes.push('process start to first emporium_query eq ok')
  return { calls, ...o }
}

export const cases = [
  // One-record write (a cell edit) onto a graph whose log already holds n events; fresh graph per case so writes do not pollute each other.
  ...['n0', 'n10k', 'n100k'].map((s) => mk(`rich.emporium_log.write.b1.${s}`, [[3, 1], [30, 3]], {
    async setup(ctx) { await logGraph(ctx, `lw1${s}`, sizeFor(ctx, s)); ctx.params.n_actual = ctx.L.n; ctx.params.n_unit = 'records (events = ceil(n/100), one emporium_write per 100-record batch)'; ctx.params.tier = tierOf(ctx) },
    async run(ctx) { const base = 1_000_000; return run(ctx, (c, i) => W(c, gizmoRecords(1, seedOf(c), base + i))) } })),
  // A 100-record batch (a CSV paste).
  ...['n0', 'n10k'].map((s) => mk(`rich.emporium_log.write.b100.${s}`, [[2, 1], [8, 2]], {
    async setup(ctx) { await logGraph(ctx, `lw100${s}`, sizeFor(ctx, s)); ctx.params.n_actual = ctx.L.n; ctx.params.n_unit = 'records (events = ceil(n/100), one emporium_write per 100-record batch)'; ctx.params.tier = tierOf(ctx) },
    async run(ctx) { const base = 1_000_000; return run(ctx, (c, i) => W(c, gizmoRecords(100, seedOf(c), base + i * 100))) } })),
  // emporium_query eq on weight after the log is folded: the projection should cost the same on both binaries.
  mk('rich.emporium_log.query_eq.n10k', [[5, 1], [30, 3]], {
    async setup(ctx) { await sharedLogGraph(ctx, 'lqe10', sizeFor(ctx, 'n10k')); ctx.params.n_actual = ctx.L.n; ctx.params.n_unit = 'records (events = ceil(n/100), one emporium_write per 100-record batch)'; ctx.params.tier = tierOf(ctx) },
    async run(ctx) { return run(ctx, (c, i) => Q(c, { weight: { eq: (i * 37) % 1000 } })) } }),
  // The oracle: fold the log into a scratch store and compare sets. equal must be true, else the iteration errors.
  ...['n10k', 'n100k'].map((s) => mk(`rich.emporium_log.replay_oracle.${s}`, [[1, 0], [3, 1]], {
    async setup(ctx) { await sharedLogGraph(ctx, s === 'n10k' ? 'lro10' : 'lro100', sizeFor(ctx, s)); ctx.params.n_actual = ctx.L.n; ctx.params.n_unit = 'records (events = ceil(n/100), one emporium_write per 100-record batch)'; ctx.params.tier = tierOf(ctx) },
    async run(ctx) {
      if (ctx.noLog) return skipRun(ctx)
      const rows = []
      const out = await run(ctx, async () => { const r = await replay(ctx, 'oracle'); rows.push({ ms: r.ms, events: r.events, triples: r.triples, equal: r.equal, missing: r.missing, extra: r.extra }); if (r.equal !== true) throw new Error(`oracle not equal: missing ${r.missing} extra ${r.extra}`) })
      out.extra.replayMs = rows.map((r) => r.ms); out.extra.events = rows.at(-1)?.events; out.extra.triples = rows.at(-1)?.triples; out.extra.equal = rows.every((r) => r.equal === true)
      out.extra.notes.push(`server replay ms: ${rows.map((r) => r.ms).join(', ')}`)
      return out
    } })),
  // Rebuild the live projection from nothing (an admin act). Clear-then-apply, so the shared graph is left equal again.
  mk('rich.emporium_log.replay_live.n100k', [[1, 0], [2, 1]], {
    async setup(ctx) { await sharedLogGraph(ctx, 'lrl100', sizeFor(ctx, 'n100k')); ctx.params.n_actual = ctx.L.n; ctx.params.n_unit = 'records (events = ceil(n/100), one emporium_write per 100-record batch)'; ctx.params.tier = tierOf(ctx) },
    async run(ctx) {
      if (ctx.noLog) return skipRun(ctx)
      const rows = []
      const out = await run(ctx, async () => { const r = await replay(ctx, 'live'); rows.push({ ms: r.ms, events: r.events, triples: r.triples, equal: r.equal }); if (r.equal !== true) throw new Error('live replay not equal') })
      out.extra.replayMs = rows.map((r) => r.ms); out.extra.events = rows.at(-1)?.events; out.extra.triples = rows.at(-1)?.triples
      out.extra.notes.push(`server replay ms: ${rows.map((r) => r.ms).join(', ')}`)
      return out
    } }),
  // Reported, not timed: log bytes on disk against the same records as JSONL (JSON.stringify(record) + newline, summed client-side).
  mk('rich.emporium_log.disk.n100k', [[1, 0], [1, 0]], {
    async setup(ctx) { await sharedLogGraph(ctx, 'ldk100', sizeFor(ctx, 'n100k')); ctx.params.n_actual = ctx.L.n; ctx.params.n_unit = 'records (events = ceil(n/100), one emporium_write per 100-record batch)'; ctx.params.tier = tierOf(ctx) },
    async run(ctx) {
      if (ctx.noLog) return skipRun(ctx)
      let st = null
      const out = await run(ctx, async () => { st = await settledStats(ctx) })
      const jsonl = ctx.L.jsonlBytes, bytes = st?.bytesOnDisk ?? 0, ratio = jsonl ? r1((bytes / jsonl) * 100) / 100 : null
      Object.assign(out.extra, { bytesOnDisk: bytes, jsonlBytes: jsonl, ratio, events: st?.events })
      out.extra.notes.push(`bytesOnDisk ${bytes}, JSONL ${jsonl} (records, not events), ratio ${ratio} (threshold <= 3)`)
      return out
    } }),
  // Process start to the first query_eq ok on a graph with the large log. Restarts on the same profile as core/boot.mjs; the last rep stays up.
  mk('rich.emporium_log.boot.first_query.n100k', [[1, 0], [3, 0]], {
    kind: 'boot', needs: ['proc'],
    async setup(ctx) {
      const g = await ctx.proc.start(); await g.waitHealthy()
      await logGraph(ctx, 'lbt100', sizeFor(ctx, 'n100k')); ctx.params.n_actual = ctx.L.n; ctx.params.n_unit = 'records (events = ceil(n/100), one emporium_write per 100-record batch)'; ctx.params.tier = tierOf(ctx)
      await ctx.proc.stop() // flush; every rep below is a cold start
    },
    async run(ctx) {
      return bootRun(ctx)
    } }),
  // ---- W4: event-scale variants. The log is seeded with single-record events pushed straight into the room (origin = the cell's own, so the hook skips them);
  // the id's e10k / e100k are nominal (tier m: 10,000 / 100,000 events; tier s: 1,000 / 10,000). ctx.L.n counts EVENTS here. A binary without the log returns {skipped}.
  ...['e10k', 'e100k'].map((s) => mk(`rich.emporium_log.write.b1.${s}`, [[3, 1], [30, 3]], {
    async setup(ctx) { await eventGraph(ctx, `lw1${s}`, evFor(ctx, s)); evParams(ctx) },
    async run(ctx) { if (ctx.noLog) return { skipped: NO_LOG }; const base = 1_000_000; return run(ctx, (c, i) => W(c, gizmoRecords(1, seedOf(c), base + i))) } })),
  // Rebuild the live projection from 100k events (an admin act). Runs first on the shared e100k graph; 900 s client timeout.
  mk('rich.emporium_log.replay_live.e100k', [[1, 0], [1, 0]], {
    async setup(ctx) { await sharedEventGraph(ctx, 'lrle100', evFor(ctx, 'e100k')); evParams(ctx) },
    async run(ctx) {
      if (ctx.noLog) return { skipped: NO_LOG }
      const rows = []
      const out = await run(ctx, async () => { const r = await replay(ctx, 'live', 900_000); rows.push({ ms: r.ms, events: r.events, triples: r.triples, equal: r.equal }); if (r.equal !== true) throw new Error(`live replay not equal: missing ${r.missing} extra ${r.extra}`) })
      Object.assign(out.extra, { replayMs: rows.map((r) => r.ms), ms: rows.at(-1)?.ms, events: rows.at(-1)?.events, triples: rows.at(-1)?.triples })
      out.extra.notes.push(`server replay ms: ${rows.map((r) => r.ms).join(', ')}; events ${rows.at(-1)?.events}, triples ${rows.at(-1)?.triples}`)
      return out
    } }),
  // The oracle on the same shared e100k graph, after the live rebuild.
  mk('rich.emporium_log.replay_oracle.e100k', [[2, 0], [2, 0]], {
    async setup(ctx) { await sharedEventGraph(ctx, 'lrle100', evFor(ctx, 'e100k')); evParams(ctx) },
    async run(ctx) {
      if (ctx.noLog) return { skipped: NO_LOG }
      const rows = []
      const out = await run(ctx, async () => { const r = await replay(ctx, 'oracle', 900_000); rows.push({ ms: r.ms, events: r.events, triples: r.triples, equal: r.equal, missing: r.missing, extra: r.extra }); if (r.equal !== true) throw new Error(`oracle not equal: missing ${r.missing} extra ${r.extra}`) })
      Object.assign(out.extra, { replayMs: rows.map((r) => r.ms), events: rows.at(-1)?.events, triples: rows.at(-1)?.triples, equal: rows.every((r) => r.equal === true) })
      out.extra.notes.push(`server replay ms: ${rows.map((r) => r.ms).join(', ')}`)
      return out
    } }),
  // Reported, not timed: bytes on disk against the pushed events as JSONL (JSON.stringify(event) + newline).
  mk('rich.emporium_log.disk.e100k', [[1, 0], [1, 0]], {
    async setup(ctx) { await sharedEventGraph(ctx, 'lrle100', evFor(ctx, 'e100k')); evParams(ctx) },
    async run(ctx) {
      if (ctx.noLog) return { skipped: NO_LOG }
      let st = null
      const out = await run(ctx, async () => { st = await settledStats(ctx) })
      const jsonl = ctx.L.jsonlBytes, bytes = st?.bytesOnDisk ?? 0, ratio = jsonl ? r1((bytes / jsonl) * 100) / 100 : null
      Object.assign(out.extra, { bytesOnDisk: bytes, jsonlBytes: jsonl, ratio, events: st?.events })
      out.extra.notes.push(`bytesOnDisk ${bytes}, JSONL ${jsonl} (pushed events), ratio ${ratio} (threshold <= 3)`)
      return out
    } }),
  // Cold start to the first query_eq on a graph holding 100k events; builds its own graph (restarts the cell, so the shared one is not used).
  mk('rich.emporium_log.boot.first_query.e100k', [[1, 0], [3, 0]], {
    kind: 'boot', needs: ['proc'],
    async setup(ctx) {
      const g = await ctx.proc.start(); await g.waitHealthy()
      await eventGraph(ctx, 'lbte100', evFor(ctx, 'e100k')); evParams(ctx)
      await ctx.proc.stop() // flush; every rep below is a cold start
    },
    async run(ctx) {
      if (ctx.noLog) return { skipped: NO_LOG }
      return bootRun(ctx)
    } }),
]
