// Shared helpers for the rich.emporium_log.* cases (file name starts with _ so the runner does not load it as a case module).
import { graphFor, sleep } from './_rich.mjs'
import { ONTOLOGY_CLASSES, logEvents, gizmoRecords } from '../../fixture/parts/rich-gen.mjs'
import * as ylog from '../../lib/ylog.mjs'

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
// timeoutMs: the client's default is 600 s; the e100k live replay asks for 900 s (own fetch, same auth).
export async function replay(ctx, into, timeoutMs = null) {
  let r
  if (timeoutMs) {
    const t0 = performance.now()
    const res = await fetch(ctx.url.replace(/\/$/, '') + logPath(ctx, '/replay'), { method: 'POST', headers: { 'content-type': 'application/json', authorization: `Bearer ${ctx.key}` }, body: JSON.stringify({ into }), signal: AbortSignal.timeout(timeoutMs) })
    const text = await res.text(); let json = null; try { json = JSON.parse(text) } catch {}
    r = { status: res.status, text: text.replaceAll(ctx.key, '<key>'), json, ms: performance.now() - t0 }
  } else r = await ctx.http('POST', logPath(ctx, '/replay'), { into })
  if (r.status >= 300) throw new Error(`replay ${into}: ${r.status} ${r.text.slice(0, 120)}`)
  return r.json
}
// bytesOnDisk once it stops growing (persistence trails the writes).
export async function settledStats(ctx, maxMs = 8000) {
  let prev = null, st = await logStats(ctx); const t0 = performance.now()
  while (st && st.bytesOnDisk !== prev && performance.now() - t0 < maxMs) { prev = st.bytesOnDisk; await sleep(500); st = await logStats(ctx) }
  return st
}

// ---- event-scale seeding (W4): single-record events pushed straight into the log room ------------------------------------------------
// Event counts by tier for the e10k / e100k ids (nominal names; the tier decides the real count).
export const EVENT_SIZES = { s: { small: 1000, large: 10000 }, m: { small: 10000, large: 100000 }, l: { small: 10000, large: 100000 } }
export const eventsOf = (ctx, which) => (EVENT_SIZES[tierOf(ctx)] ?? EVENT_SIZES.s)[which]

// Seed graph g (ctx.graphId, ontology + mode already on) with nEvents single-record events shaped like a real logged event.
// 1. read origin = GET stats .origin while the log is empty (the cell's own origin, so the hook skips pushed events), unless `origin` is given;
// 2. ONE real emporium_write of one gizmoRecord, read back through the room (events[0]) as the template;
// 3. clone it per event: distinct subject (trailing localId segment replaced), fields from gizmoRecords, at_ms increasing;
// 4. pushEvents, then poll GET stats until events == nEvents + 1 (timeout 300 s). Returns ms for the whole seeding; ctx.lastSeed = {ms, jsonlBytes, pushMs, template}.
export async function seedEventsDirect(ctx, g, nEvents, origin = null) {
  if (!ylog.available) throw new Error('yjs / y-protocols not installed (pnpm install in bench/)')
  const t0 = performance.now(), stats = async () => (await ctx.http('GET', `/emporium/log/${g}/workspace`)).json
  if (origin == null) {
    const st = await stats()
    if (!st || (st.events ?? 0) !== 0) throw new Error(`seedEventsDirect: log of ${g} is not empty (${st?.events})`)
    origin = st.origin
  }
  const seed = seedOf(ctx), base = 2_000_000
  await ctx.mcp('emporium_write', { graphId: g, vocab: 'workspace', records: gizmoRecords(1, seed, base) })
  const c = ylog.connect(ctx.url, ctx.key, g, 'workspace')
  try {
    await c.synced()
    const arr = c.ydoc.getArray('events'), tw = performance.now()
    while (arr.length < 1) { if (performance.now() - tw > 30_000) throw new Error('template event never appeared in the log room'); await sleep(20) }
    const raw = arr.get(0), template = JSON.parse(JSON.stringify(typeof raw?.toJSON === 'function' ? raw.toJSON() : raw, (_k, v) => (typeof v === 'bigint' ? Number(v) : v)))
    const rec0 = template.records?.[0]
    ctx.log({ templateSample: JSON.stringify(template).slice(0, 600) })
    if (!rec0 || rec0.subject == null) throw new Error('template event has no records[0].subject: ' + JSON.stringify(template).slice(0, 200))
    const lastSeg = /[^/#:]+$/, atBase = Number(template.at_ms ?? Date.now())
    const mkFields = (gr) => { const { kind, clientRef, ...f } = gr; return f }
    const withId = (f, k) => ({ localId: `bench-e${k}`, ...f })
    const events = []; let jsonlBytes = 0
    const jstr = (o) => JSON.stringify(o, (_k, v) => (typeof v === 'bigint' ? Number(v) : v))
    { const tf = Object.keys(rec0.fields ?? {}).sort().join(','), cf = Object.keys(withId(mkFields(gizmoRecords(1, seed, base + 1)[0]), 0)).sort().join(','); if (tf !== cf) throw new Error(`template fields [${tf}] != clone fields [${cf}]`) }
    for (let s = 0; s < nEvents; s += 1000) {
      const recs = gizmoRecords(Math.min(1000, nEvents - s), seed, base + 1 + s)
      for (let j = 0; j < recs.length; j++) {
        const k = s + j, ev = JSON.parse(JSON.stringify(template))
        ev.at_ms = BigInt(atBase + 1 + k)
        ev.records[0].subject = String(rec0.subject).replace(lastSeg, `bench-e${k}`)
        ev.records[0].fields = withId(mkFields(recs[j]), k)
        events.push(ev); jsonlBytes += Buffer.byteLength(jstr(ev) + '\n')
      }
    }
    const tp = performance.now()
    await ylog.pushEvents(c, events, { chunk: 2000, meta: { graphId: g, vocab: 'workspace', origin: template.origin ?? origin } })
    const pushMs = Math.round(performance.now() - tp)
    const want = nEvents + 1, td = performance.now()
    for (;;) {
      const st = await stats()
      if ((st?.events ?? 0) >= want) break
      if (performance.now() - td > 300_000) throw new Error(`log of ${g} holds ${st?.events} events after 300 s, wanted ${want}`)
      await sleep(250)
    }
    const ms = Math.round(performance.now() - t0)
    ctx.lastSeed = { ms, pushMs, jsonlBytes: jsonlBytes + Buffer.byteLength(jstr(template) + '\n'), template }
    return ms
  } finally { c.close() }
}

// Like logGraph, but the n events are single-record events pushed directly (seconds, not hours). ctx.L.n counts EVENTS here (nUnit 'events').
export async function eventGraph(ctx, name, nEvents) {
  await graphFor(ctx, name)
  await ctx.mcp('propose_domain_ontology', { graphId: ctx.graphId, vocab: 'workspace', classes: ONTOLOGY_CLASSES(), rationale: 'gbench' })
  const m = await ctx.http('PUT', logPath(ctx, '/mode'), { mode: 'on' })
  const notes = []
  ctx.noLog = false
  if (m.status === 404) { ctx.noLog = true; notes.push(NO_LOG + ': mode route 404, nothing seeded') }
  else if (m.status >= 300) throw new Error(`log mode on: ${m.status} ${m.text.slice(0, 120)}`)
  let seedMs = 0, jsonlBytes = 0
  if (!ctx.noLog) { seedMs = await seedEventsDirect(ctx, ctx.graphId, nEvents); jsonlBytes = ctx.lastSeed.jsonlBytes; notes.push(`seeded ${nEvents} single-record events by room push (+1 real write), push ${ctx.lastSeed.pushMs} ms`)
    const rp = await replay(ctx, 'live', 900_000); notes.push(`projection built by live replay (pushed events carry the cell's own origin): ${rp.ms} ms, events ${rp.events}, triples ${rp.triples}`) }
  ctx.L = { n: nEvents, nUnit: 'events', graphId: ctx.graphId, noLog: ctx.noLog, seedMs, jsonlBytes, notes }
  ctx.log({ eventGraph: ctx.graphId, nEvents, seedMs, noLog: ctx.noLog })
  return ctx.L
}
// Built once per nEvents per run and reused (ctx.shared is per family): replay_live, replay_oracle and disk share the e100k graph.
export async function sharedEventGraph(ctx, name, nEvents) {
  const cache = (ctx.shared.emlogE ??= {})
  if (cache[nEvents]) { const L = cache[nEvents]; ctx.graphId = L.graphId; ctx.noLog = L.noLog; ctx.L = L; return L }
  return (cache[nEvents] = await eventGraph(ctx, name, nEvents))
}
