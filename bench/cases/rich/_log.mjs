// Shared helpers for the rich.emporium_log.* cases (file name starts with _ so the runner does not load it as a case module).
import { graphFor, sleep } from './_rich.mjs'
import { ONTOLOGY_CLASSES, logEvents } from '../../fixture/parts/rich-gen.mjs'

// Actual n by fixture tier: the case ids keep the spec's n10k / n100k names, the tier decides the real size.
export const SIZES = { s: { small: 1000, large: 10000 }, m: { small: 10000, large: 100000 }, l: { small: 10000, large: 100000 } }
export const tierOf = (ctx) => ctx.fixture?.tier ?? 's'
export const sizeOf = (ctx, which) => (SIZES[tierOf(ctx)] ?? SIZES.s)[which]
export const seedOf = (ctx) => ctx.fixture?.seed ?? 1
export const NO_LOG = 'no log on this binary'
export const W = (ctx, records) => ctx.mcp('emporium_write', { graphId: ctx.graphId, vocab: 'workspace', records })
export const Q = (ctx, criteria, limit = 25) => ctx.mcp('emporium_query', { graphId: ctx.graphId, vocab: 'workspace', class: 'Gizmo', criteria, limit })
export const logPath = (ctx, tail = '') => `/emporium/log/${ctx.graphId}/workspace${tail}`

// A dedicated graph with the Gizmo ontology, the log mode switched on, and n events (batches of 100, one event per emporium_write call).
// A 404 on the mode route means the binary has no log: ctx.noLog = true, and the events are still written (today's path).
// Sets ctx.graphId, ctx.noLog, ctx.L = { n, graphId, noLog, seedMs, jsonlBytes, notes }; returns ctx.L.
export async function logGraph(ctx, name, n) {
  await graphFor(ctx, name)
  await ctx.mcp('propose_domain_ontology', { graphId: ctx.graphId, vocab: 'workspace', classes: ONTOLOGY_CLASSES(), rationale: 'gbench' })
  const m = await ctx.http('PUT', logPath(ctx, '/mode'), { mode: 'on' })
  const notes = []
  ctx.noLog = false
  if (m.status === 404) { ctx.noLog = true; notes.push(NO_LOG + ': mode route 404, measuring today\'s path') }
  else if (m.status >= 300) throw new Error(`log mode on: ${m.status} ${m.text.slice(0, 120)}`)
  let jsonlBytes = 0
  const t0 = performance.now()
  for (const batch of logEvents(n, seedOf(ctx), 0)) {
    await W(ctx, batch)
    for (const r of batch) jsonlBytes += Buffer.byteLength(JSON.stringify(r) + '\n')
  }
  const seedMs = Math.round(performance.now() - t0)
  ctx.L = { n, graphId: ctx.graphId, noLog: ctx.noLog, seedMs, jsonlBytes, notes }
  ctx.log({ logGraph: ctx.graphId, n, seedMs, noLog: ctx.noLog })
  return ctx.L
}
// Same, but built once per n and reused by the read-mostly cases of the family (ctx.shared is per family).
export async function sharedLogGraph(ctx, name, n) {
  const cache = (ctx.shared.emlog ??= {})
  if (cache[n]) { const L = cache[n]; ctx.graphId = L.graphId; ctx.noLog = L.noLog; ctx.L = L; return L }
  return (cache[n] = await logGraph(ctx, name, n))
}
// GET stats: {graphId, vocab, events, bytesOnDisk, lastAtMs, origin}; null on a binary without the log.
export async function logStats(ctx) {
  if (ctx.noLog) return null
  const r = await ctx.http('GET', logPath(ctx))
  if (r.status >= 300) throw new Error(`log stats: ${r.status} ${r.text.slice(0, 120)}`)
  return r.json
}
// POST replay {into: 'oracle' | 'live'} -> {events, triples, equal, missing, extra, ms, ...}
export async function replay(ctx, into) {
  const r = await ctx.http('POST', logPath(ctx, '/replay'), { into })
  if (r.status >= 300) throw new Error(`replay ${into}: ${r.status} ${r.text.slice(0, 120)}`)
  return r.json
}
// bytesOnDisk once it stops growing (persistence trails the writes).
export async function settledStats(ctx, maxMs = 8000) {
  let prev = null, st = await logStats(ctx); const t0 = performance.now()
  while (st && st.bytesOnDisk !== prev && performance.now() - t0 < maxMs) { prev = st.bytesOnDisk; await sleep(500); st = await logStats(ctx) }
  return st
}
