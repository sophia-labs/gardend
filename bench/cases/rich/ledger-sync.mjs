// rich.ledger.*: the ledger room (/hocuspocus/ledger/{graph}/{vocab}) between two spawned gardends (--twin) and on one cell.
// The bench is the relay a browser is: ledger.relay(X, Y) carries the frames X's cell ships into Y, which sends them to Y's cell.
// On a binary without the ledger (the version route 404s) each case is skipped with a note.
// Suites include 'smoke' (unlike rich.emporium_log.*) because the W6 gate is `gbench run --suite smoke --cases 'rich.ledger' --twin`.
import { IT, SUITES, graphFor, sleep } from './_rich.mjs'
import { sizeOf, tierOf } from './_log.mjs'
import * as ledger from '../../lib/ledger.mjs'

const LSUITES = ['smoke', ...SUITES]
const VOCAB = 'workspace'
const NO_LEDGER = 'no ledger on this binary (version route 404); nothing to measure'
const NO_DEPS = '@noble/hashes not installed (pnpm install in bench/)'
const TIMEOUT = 120_000
const timeoutOf = (what, ms) => new Promise((_, rej) => setTimeout(() => rej(new Error(`timeout ${ms} ms: ${what}`)), ms))
const primary = (ctx) => ({ url: ctx.url, key: ctx.key, http: (m, p, b) => ctx.http(m, p, b) })

// Same graph id on the primary (graphFor) and, when `twin`, on the twin. Returns the id, or null when the binary has no ledger.
async function pair(ctx, name, { twin = true } = {}) {
  const g = await graphFor(ctx, name)
  if (twin) {
    const r = await ctx.twin.http('POST', '/api/graphs', { title: g, graph_id: g })
    if (r.status >= 300 && !/exist|already/i.test(r.text)) throw new Error(`twin create graph ${g}: ${r.status}`)
  }
  if ((await ledger.version(primary(ctx), g, VOCAB)) === null) return null
  return g
}
// a small, deterministic event: one Gizmo subject per frame
const draftOf = (i, tag = 'led') => ({ witness: 'urn:gbench:witness', subjects: [{ subject: `urn:gbench:${tag}:${i}`, class: 'urn:gbench:Gizmo', parents: [] }], mode: 'assert', payload: JSON.stringify([{ s: `urn:gbench:${tag}:${i}`, p: 'urn:gbench:n', o: i }]) })
async function until(what, fn, ms = TIMEOUT, step = 2) { const t0 = performance.now(); for (;;) { if (await fn()) return performance.now() - t0; if (performance.now() - t0 > ms) throw new Error(`timeout ${ms} ms: ${what}`); await sleep(step) } }

const cold = (id, which) => ({
  id, version: 1, kind: 'rich', suites: which === 'small' ? LSUITES : SUITES, params: IT([1, 0], [3, 1]), needs: ['twin'],
  async run(ctx) {
    if (!ledger.available) return { skipped: NO_DEPS }
    const n = sizeOf(ctx, which); ctx.params.n = n; ctx.params.n_actual = n; ctx.params.tier = tierOf(ctx)
    ctx.params.def = 'n frames appended to cell A by a seeder client (then closed); a fresh client X cold-syncs A, relayed into client Y on cell B; ms from the relay start until cell B version.origins[seeder] == n'
    const calls = []
    for (let i = 0; i < ctx.params.iterations + ctx.params.warmup; i++) {
      const g = await pair(ctx, `lc${which[0]}${i}`); if (g === null) return { skipped: NO_LEDGER }
      const seedOrigin = `seed${i}-${Math.random().toString(36).slice(2, 7)}`
      let X = null, Y = null, ok = true, error = null, ms = 0
      try {
        const S = ledger.connect(ctx.url, ctx.key, g, VOCAB, seedOrigin)
        try { await S.synced(); await ledger.appendMany(S, n, (j) => draftOf(j)); await ledger.waitVersion(primary(ctx), g, VOCAB, (v) => (v.durable?.[seedOrigin] ?? 0) >= n, { timeoutMs: TIMEOUT }) } finally { S.close() }
        X = ledger.connect(ctx.url, ctx.key, g, VOCAB, `a${i}-${Math.random().toString(36).slice(2, 7)}`)
        Y = ledger.connect(ctx.twin.url, ctx.twin.key, g, VOCAB, `b${i}-${Math.random().toString(36).slice(2, 7)}`)
        await Promise.all([X.synced(), Y.synced()])
        await until(`client X holds ${n} frames of ${seedOrigin}`, () => ledger.lastSeq(X, seedOrigin) >= n)
        const t0 = performance.now(); const off = ledger.relay(X, Y)
        try { await Promise.race([ledger.waitVersion(ctx.twin, g, VOCAB, (v) => (v.origins?.[seedOrigin] ?? 0) >= n, { timeoutMs: TIMEOUT }), timeoutOf('cell B never reached n frames', TIMEOUT)]); ms = performance.now() - t0 } finally { off() }
        if (X.errors.length || Y.errors.length) throw new Error(`client errors: ${[...X.errors, ...Y.errors].slice(0, 2).join('; ')}`)
      } catch (e) { ok = false; error = ledger.available ? String(e.message).slice(0, 200) : NO_DEPS; ms = TIMEOUT } finally { X?.close(); Y?.close() }
      if (i >= ctx.params.warmup) calls.push({ ms, ok, traceId: null, error })
    }
    return { calls }
  } })

export const cases = [
  // Cold replica: the physics of the ledger sync (ARCHITECTURE.md section 4). n10k / n100k are tier-scaled names, as in rich.emporium_log.sync.*.
  cold('rich.ledger.sync.cold.n10k', 'small'),
  cold('rich.ledger.sync.cold.n100k', 'large'),
  // With the pair synced and the relay live: one frame appended by a writer on cell A; ms until cell B's version.origins[writer] increments.
  { id: 'rich.ledger.sync.delta', version: 1, kind: 'rich', suites: LSUITES, params: IT([3, 1], [20, 3]), needs: ['twin'],
    async run(ctx) {
      if (!ledger.available) return { skipped: NO_DEPS }
      const base = 100; ctx.params.n = base; ctx.params.n_actual = base; ctx.params.tier = tierOf(ctx)
      ctx.params.def = 'one frame appended by a writer client on cell A with the relay live (subscriber X on A -> Y on B); ms from the append until cell B version.origins[writer] increments'
      const g = await pair(ctx, 'ld'); if (g === null) return { skipped: NO_LEDGER }
      const wo = `w-${Math.random().toString(36).slice(2, 7)}`
      const W = ledger.connect(ctx.url, ctx.key, g, VOCAB, wo), calls = []
      let X = null, Y = null
      try {
        await W.synced(); await ledger.appendMany(W, base, (j) => draftOf(j, 'ld'))
        await ledger.waitVersion(primary(ctx), g, VOCAB, (v) => (v.durable?.[wo] ?? 0) >= base, { timeoutMs: TIMEOUT })
        X = ledger.connect(ctx.url, ctx.key, g, VOCAB, `x-${Math.random().toString(36).slice(2, 7)}`); Y = ledger.connect(ctx.twin.url, ctx.twin.key, g, VOCAB, `y-${Math.random().toString(36).slice(2, 7)}`)
        await Promise.all([X.synced(), Y.synced()])
        await until(`X holds ${base} frames`, () => ledger.lastSeq(X, wo) >= base)
        const off = ledger.relay(X, Y)
        await ledger.waitVersion(ctx.twin, g, VOCAB, (v) => (v.origins?.[wo] ?? 0) >= base, { timeoutMs: TIMEOUT })
        let have = base
        for (let i = 0; i < ctx.params.iterations + ctx.params.warmup; i++) {
          let ok = true, error = null, ms = 0
          const t0 = performance.now()
          try {
            ledger.append(W, draftOf(base + i, 'ld')); have += 1
            for (;;) { const v = await ledger.version(ctx.twin, g, VOCAB); if ((v?.origins?.[wo] ?? 0) >= have) { ms = performance.now() - t0; break } if (performance.now() - t0 > 30_000) throw new Error('timeout 30000 ms: cell B version did not increment'); await sleep(1) }
          } catch (e) { ok = false; error = String(e.message).slice(0, 200); ms = performance.now() - t0 }
          if (i >= ctx.params.warmup) calls.push({ ms, ok, traceId: null, error })
        }
        off()
        if (W.errors.length || X.errors.length || Y.errors.length) return { calls: [...calls, { ms: 0, ok: false, traceId: null, error: `client errors: ${[...W.errors, ...X.errors, ...Y.errors].slice(0, 2).join('; ')}` }] }
      } finally { W.close(); X?.close(); Y?.close() }
      return { calls }
    } },
  // Awareness on the ledger room: client 1 publishes a state at 20/s, client 2 on the same room receives it; ms from send to receive.
  { id: 'rich.ledger.awareness.echo', version: 1, kind: 'rich', suites: LSUITES, params: { iterations: 600, warmup: 0, concurrency: 1, hz: 20, smoke: { iterations: 100 }, def: 'awareness state sent by client 1 on the ledger room; ms until client 2 holds it' }, needs: [],
    async run(ctx) {
      if (!ledger.available) return { skipped: NO_DEPS }
      const n = ctx.params.iterations, hz = ctx.params.hz
      const g = await pair(ctx, 'la', { twin: false }); if (g === null) return { skipped: NO_LEDGER }
      const c1 = ledger.connect(ctx.url, ctx.key, g, VOCAB), c2 = ledger.connect(ctx.url, ctx.key, g, VOCAB)
      try {
        await Promise.all([c1.synced(), c2.synced()]); await sleep(300)
        const sent = new Map(), calls = []
        const off = ledger.onAwareness(c2, (states) => { const now = performance.now(); for (const s of states) { const t = sent.get(s.state?.seq); if (t !== undefined) { sent.delete(s.state.seq); calls.push({ ms: now - t, ok: true, traceId: null }) } } })
        const period = 1000 / hz, start = performance.now()
        for (let i = 0; i < n; i++) {
          const d = start + i * period - performance.now(); if (d > 0) await sleep(d)
          sent.set(i, performance.now()); ledger.send(c1, { seq: i, user: { name: 'gbench' } })
        }
        const dead = performance.now() + 5000; while (sent.size && performance.now() < dead) await sleep(20)
        off()
        const lost = sent.size
        return { calls: [...calls, ...Array.from({ length: lost }, () => ({ ms: 5000, ok: false, traceId: null }))], extra: { sent: n, lost, hz } }
      } finally { c1.close(); c2.close(); await sleep(300) }
    } },
]
