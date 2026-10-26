// Join gardend's OTel JSON-lines spans to the runner's calls by trace id.
import { createReadStream, existsSync } from 'node:fs'
import { createInterface } from 'node:readline'
import { quantile } from './stats.mjs'

const ms = (s) => (s.end_unix_nanos - s.start_unix_nanos) / 1e6
const sorted = (xs) => [...xs].sort((a, b) => a - b)
const pq = (xs, q) => quantile(sorted(xs), q)
const rd = (x) => Math.round(x * 100) / 100

// Read span files; keep only spans whose trace id is in `wanted` (a Set). Count the rest by name.
export async function loadSpans(files, wanted) {
  const byTrace = new Map(), other = new Map(); let total = 0, bad = 0
  for (const f of files) {
    if (!existsSync(f)) continue
    const rl = createInterface({ input: createReadStream(f), crlfDelay: Infinity })
    for await (const line of rl) {
      if (!line) continue
      let s; try { s = JSON.parse(line) } catch { bad++; continue }
      total++
      if (wanted.has(s.trace_id)) { let a = byTrace.get(s.trace_id); if (!a) byTrace.set(s.trace_id, (a = [])); a.push(s) }
      else { const o = other.get(s.name) ?? { n: 0, totalMs: 0 }; o.n++; o.totalMs += ms(s); other.set(s.name, o) }
    }
  }
  return { byTrace, other, total, bad }
}

const fsyncEvents = (s) => (s.events ?? []).filter((e) => e.name === 'fsync')

// Per-call server facts from the spans of one trace.
export function analyzeTrace(spans) {
  const kids = new Map()
  for (const s of spans) { const a = kids.get(s.parent_span_id) ?? []; a.push(s); kids.set(s.parent_span_id, a) }
  let fsyncs = 0, fsyncMs = 0, leaseWait = 0, leaseHold = 0, queueWait = 0
  const self = new Map()
  for (const s of spans) {
    const ev = fsyncEvents(s); fsyncs += ev.length; for (const e of ev) fsyncMs += e.attributes?.['sophia.fsync.ms'] ?? 0
    const a = s.attributes ?? {}
    if (s.name.startsWith('graph.lease.')) { leaseWait += a['sophia.lease.wait_ms'] ?? 0; leaseHold = Math.max(leaseHold, a['sophia.lease.held_ms'] ?? 0) }
    if (a['sophia.queue.wait_ms'] != null && a['sophia.operation.recovered'] !== true) queueWait += a['sophia.queue.wait_ms']
    // self time = duration minus the union of child intervals (children may overlap)
    const cs = (kids.get(s.span_id) ?? []).map((c) => [c.start_unix_nanos, c.end_unix_nanos]).sort((x, y) => x[0] - y[0])
    let covered = 0, curS = null, curE = null
    for (const [st, en] of cs) { const a0 = Math.max(st, s.start_unix_nanos), b0 = Math.min(en, s.end_unix_nanos); if (b0 <= a0) continue; if (curE === null || a0 > curE) { if (curE !== null) covered += curE - curS; curS = a0; curE = b0 } else curE = Math.max(curE, b0) }
    if (curE !== null) covered += curE - curS
    const selfMs = Math.max(0, (s.end_unix_nanos - s.start_unix_nanos - covered) / 1e6)
    const e = self.get(s.name) ?? { selfMsTotal: 0, n: 0 }; e.selfMsTotal += selfMs; e.n++; self.set(s.name, e)
  }
  return { fsyncs, fsyncMs, leaseWait, leaseHold, queueWait, self, nSpans: spans.length }
}

const KEEP = /^(sophia\.|otel\.|http\.|rpc\.|db\.)/
export function trimSpans(spans, limit = 300) {
  const t0 = Math.min(...spans.map((s) => s.start_unix_nanos))
  return [...spans].sort((a, b) => a.start_unix_nanos - b.start_unix_nanos).slice(0, limit).map((s) => {
    const attrs = {}
    for (const [k, v] of Object.entries(s.attributes ?? {})) if (KEEP.test(k) && (typeof v === 'number' || typeof v === 'boolean' || (typeof v === 'string' && v.length <= 80))) attrs[k] = typeof v === 'number' ? rd(v) : v
    return { spanId: s.span_id, parentSpanId: s.parent_span_id, name: s.name, startOffsetMs: rd((s.start_unix_nanos - t0) / 1e6), durationMs: rd(ms(s)), fsyncs: fsyncEvents(s).length, attrs }
  })
}

// Build CaseResult.server and exemplars from calls [{traceId, ms, ok}] and the loaded spans.
export function serverFacts(calls, loaded) {
  const per = []
  let joined = 0
  const selfAgg = new Map()
  for (const c of calls) {
    const spans = loaded.byTrace.get(c.traceId)
    if (!spans) continue
    joined++
    const a = analyzeTrace(spans); per.push({ call: c, a, spans })
    for (const [name, v] of a.self) { const e = selfAgg.get(name) ?? { selfMsTotal: 0, n: 0 }; e.selfMsTotal += v.selfMsTotal; e.n += v.n; selfAgg.set(name, e) }
  }
  if (!per.length) return { server: null, exemplars: [], joined: 0 }
  const col = (f) => per.map((p) => f(p.a))
  const dist3 = (xs) => ({ p50: rd(pq(xs, 0.5)), p95: rd(pq(xs, 0.95)), max: rd(Math.max(...xs)) })
  const server = {
    fsyncsPerOp: { p50: pq(col((a) => a.fsyncs), 0.5), max: Math.max(...col((a) => a.fsyncs)) },
    fsyncMsPerOp: { p50: rd(pq(col((a) => a.fsyncMs), 0.5)) },
    leaseWaitMs: dist3(col((a) => a.leaseWait)), leaseHoldMs: dist3(col((a) => a.leaseHold)), queueWaitMs: dist3(col((a) => a.queueWait)),
    topSelfTime: [...selfAgg.entries()].map(([name, v]) => ({ name, selfMsTotal: rd(v.selfMsTotal), n: v.n })).sort((x, y) => y.selfMsTotal - x.selfMsTotal).slice(0, 10),
  }
  const exemplars = per.filter((p) => p.call.ok).sort((x, y) => y.call.ms - x.call.ms).slice(0, 3).map((p) => ({ traceId: p.call.traceId, wallMs: rd(p.call.ms), spans: trimSpans(p.spans) }))
  return { server, exemplars, joined }
}

// One-paragraph text rendering of an exemplar tree.
export function renderTree(ex, maxLines = 24) {
  const kids = new Map()
  for (const s of ex.spans) { const a = kids.get(s.parentSpanId) ?? []; a.push(s); kids.set(s.parentSpanId, a) }
  const ids = new Set(ex.spans.map((s) => s.spanId)), roots = ex.spans.filter((s) => !ids.has(s.parentSpanId))
  const lines = []
  const walk = (s, d) => { if (lines.length >= maxLines) return; lines.push(`${'  '.repeat(d)}${s.name} ${s.durationMs} ms${s.fsyncs ? ` (${s.fsyncs} fsync)` : ''} @+${s.startOffsetMs}`); for (const c of kids.get(s.spanId) ?? []) walk(c, d + 1) }
  for (const r of roots) walk(r, 0)
  return lines.join('\n')
}
