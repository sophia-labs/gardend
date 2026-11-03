// rich.emporium_log.sync.* and awareness.echo: two spawned gardends (--twin), the bench is the y-websocket relay between their log rooms.
// On a binary without the log (A: the mode route 404s) each case is skipped with a note: there is no log to measure.
import { IT, SUITES, graphFor, sleep } from './_rich.mjs'
import { ONTOLOGY_CLASSES, gizmoRecords } from '../../fixture/parts/rich-gen.mjs'
import { sizeOf, tierOf, NO_LOG as NO_LOG_NOTE } from './_log.mjs'
import * as ylog from '../../lib/ylog.mjs'

const NO_LOG = `${NO_LOG_NOTE} (mode route 404); nothing to measure`
const seed = (ctx) => ctx.fixture.seed ?? 1
const nFor = (ctx) => sizeOf(ctx, 'small')
const STATS = (g) => `/emporium/log/${g}/workspace`
const events = async (api, g) => (await api.http('GET', STATS(g))).json?.events ?? -1
const count = async (api, g) => { const r = await api.c.call('emporium_query', { graphId: g, vocab: 'workspace', class: 'Gizmo', limit: 1 }); return r.ok ? (r.payload?.totalMatched ?? -1) : -1 }
const timeoutOf = (what, ms) => new Promise((_, rej) => setTimeout(() => rej(new Error(`timeout ${ms} ms: ${what}`)), ms))
// the primary cell (ctx) as a log-capable api, shaped like ctx.twin
const primary = (ctx) => ({ url: ctx.url, key: ctx.key, c: ctx.c, http: (m, p, b) => ctx.http(m, p, b), mcp: (t, a) => ctx.mcp(t, a) })

// Same graph id on both cells, same ontology, mode on, then `n` Gizmo events on the primary (batches of 100). Returns the graph id, or null when the binary has no log.
async function pair(ctx, name, n, { twin = true } = {}) {
  const g = await graphFor(ctx, name)
  const prep = async (api) => {
    const r = await api.http('POST', '/api/graphs', { title: g, graph_id: g }); if (r.status >= 300 && !/exist|already/i.test(r.text)) throw new Error(`create graph ${g}: ${r.status}`)
    await api.mcp('propose_domain_ontology', { graphId: g, vocab: 'workspace', classes: ONTOLOGY_CLASSES(), rationale: 'gbench' })
    return api.http('PUT', STATS(g) + '/mode', { mode: 'on' })
  }
  const a = primary(ctx), m = await prep(a)
  if (m.status === 404) return null
  if (m.status >= 300) throw new Error(`mode on ${g}: ${m.status} ${m.text.slice(0, 120)}`)
  if (twin) { const mb = await prep(ctx.twin); if (mb.status >= 300) throw new Error(`twin mode on ${g}: ${mb.status}`) }
  for (let s = 0; s < n; s += 100) await ctx.mcp('emporium_write', { graphId: g, vocab: 'workspace', records: gizmoRecords(Math.min(100, n - s), seed(ctx), s) })
  return { g, events: Math.ceil(n / 100) }
}
const mk = (id, it, extra) => ({ id, version: 1, kind: 'rich', suites: SUITES, params: IT(...it), needs: ['twin'], ...extra })

export const cases = [
  // Time from the relay send to B answering with all n events: B's stats say events == n and emporium_query on B counts n.
  mk('rich.emporium_log.sync.n10k', [[1, 0], [3, 1]], {
    async run(ctx) {
      if (!ylog.available) return { skipped: 'yjs / y-protocols not installed (pnpm install in bench/)' }
      const n = nFor(ctx); ctx.params.n = n; ctx.params.n_actual = n; ctx.params.tier = tierOf(ctx); ctx.params.events = Math.ceil(n / 100); ctx.params.def = 'n records written as ceil(n/100) emporium_write events (batches of 100); relay A log room state into B; ms until B stats events == ceil(n/100) and emporium_query count == n records'
      const calls = []
      for (let i = 0; i < ctx.params.iterations + ctx.params.warmup; i++) {
        const pr = await pair(ctx, `sy${i}`, n); if (pr === null) return { skipped: NO_LOG }
        const g = pr.g, want = pr.events
        const A = ylog.connect(ctx.url, ctx.key, g, 'workspace'), B = ylog.connect(ctx.twin.url, ctx.twin.key, g, 'workspace')
        let ok = true, error = null, ms = 0
        try {
          await Promise.all([A.synced(), B.synced()])
          if (A.ydoc.getArray('events').length !== want) throw new Error(`A room holds ${A.ydoc.getArray('events').length} events, wanted ${want}`)
          const t0 = performance.now(); ylog.relayState(A, B)
          let stop = false
          const done = (async () => { for (; !stop;) { if ((await events(ctx.twin, g)) >= want && (await count(ctx.twin, g)) === n) return performance.now() - t0; await sleep(5) } })()
          try { ms = await Promise.race([done, timeoutOf('B never reached n events', 120_000)]) } finally { stop = true }
        } catch (e) { ok = false; error = String(e.message).slice(0, 200); ms = 120_000 } finally { A.close(); B.close() }
        if (i >= ctx.params.warmup) calls.push({ ms, ok, traceId: null, error })
      }
      return { calls }
    } }),
  // With the pair synced and the relay live: one write on A; ms from issuing it until B's stats events increments.
  mk('rich.emporium_log.sync.delta.b1', [[3, 1], [20, 3]], {
    async run(ctx) {
      if (!ylog.available) return { skipped: 'yjs / y-protocols not installed (pnpm install in bench/)' }
      const base = 100; ctx.params.n = base; ctx.params.n_actual = base; ctx.params.tier = tierOf(ctx); ctx.params.def = 'one emporium_write on A with the relay live; ms from issuing the write until B stats events increments'
      const pr = await pair(ctx, 'sd', base); if (pr === null) return { skipped: NO_LOG }
      const g = pr.g
      const A = ylog.connect(ctx.url, ctx.key, g, 'workspace'), B = ylog.connect(ctx.twin.url, ctx.twin.key, g, 'workspace'), calls = []
      try {
        await Promise.all([A.synced(), B.synced()])
        ylog.relayState(A, B); const off = ylog.relayUpdates(A, B)
        let have = pr.events; const t00 = performance.now()
        while ((have = await events(ctx.twin, g)) < pr.events) { if (performance.now() - t00 > 120_000) throw new Error('B never reached the base events'); await sleep(5) }
        for (let i = 0; i < ctx.params.iterations + ctx.params.warmup; i++) {
          let ok = true, error = null, ms = 0
          const t0 = performance.now()
          try {
            await ctx.mcp('emporium_write', { graphId: g, vocab: 'workspace', records: gizmoRecords(1, seed(ctx), 100_000 + i) })
            for (;;) { const e = await events(ctx.twin, g); if (e > have) { have = e; ms = performance.now() - t0; break } if (performance.now() - t0 > 30_000) throw new Error('timeout 30000 ms: B events did not increment'); await sleep(1) }
          } catch (e) { ok = false; error = String(e.message).slice(0, 200); ms = performance.now() - t0 }
          if (i >= ctx.params.warmup) calls.push({ ms, ok, traceId: null, error })
        }
        off()
      } finally { A.close(); B.close() }
      return { calls }
    } }),
  // Awareness on the log room (the ws.typing.book mechanism): client 1 publishes a state at 20/s for 30 s, client 2 on the same room receives it; ms from send to receive.
  { id: 'rich.emporium_log.awareness.echo', version: 1, kind: 'rich', suites: SUITES, params: { iterations: 600, warmup: 0, concurrency: 1, hz: 20, smoke: { iterations: 100 }, def: 'awareness state sent by client 1 on the log room; ms until client 2 holds it' }, needs: [],
    async run(ctx) {
      if (!ylog.available) return { skipped: 'yjs / y-protocols not installed (pnpm install in bench/)' }
      const n = ctx.params.iterations, hz = ctx.params.hz
      const pr = await pair(ctx, 'sa', 0, { twin: false }); if (pr === null) return { skipped: NO_LOG }
      const g = pr.g
      const c1 = ylog.connect(ctx.url, ctx.key, g, 'workspace'), c2 = ylog.connect(ctx.url, ctx.key, g, 'workspace')
      try {
        await Promise.all([c1.synced(), c2.synced()]); await sleep(300)
        const sent = new Map(), calls = []
        const off = ylog.onAwareness(c2, (states) => { const now = performance.now(); for (const s of states) { const t = sent.get(s.state?.seq); if (t !== undefined) { sent.delete(s.state.seq); calls.push({ ms: now - t, ok: true, traceId: null }) } } })
        const period = 1000 / hz, start = performance.now()
        for (let i = 0; i < n; i++) {
          const d = start + i * period - performance.now(); if (d > 0) await sleep(d)
          sent.set(i, performance.now()); ylog.send(c1, { seq: i, user: { name: 'gbench' } })
        }
        const dead = performance.now() + 5000; while (sent.size && performance.now() < dead) await sleep(20)
        off()
        const lost = sent.size
        return { calls: [...calls, ...Array.from({ length: lost }, () => ({ ms: 5000, ok: false, traceId: null }))], extra: { sent: n, lost, hz } }
      } finally { c1.close(); c2.close(); await sleep(300) }
    } },
]
