// The HTTP client of the W7 commit seam and the fold routes (plans/2026-11-02-emporium-log/W7.md sections 4 and 5).
//   POST /emporium/commit/{graph}/{vocab}[?wait=folded]   body {witness, writer?, txn?, commit?, ops[]}  -> {frames:[{origin,seq,id,hlc}], committed, folded}
//   GET  /emporium/commit/{graph}/{vocab}/watermark        -> {watermark:{origin:seq}, version:{origin:seq}}
//   GET  /emporium/fold/{graph}/{vocab}/status             -> {watermark, version, lag_frames, open_txns, quarantined, contested, sink}
//   GET  /emporium/fold/{graph}/{vocab}/heads?subject=     -> head rows of one subject
//   POST /emporium/fold/{graph}/{vocab}/replay             body {into: 'oracle' | 'live'}
// `api` is anything with {url, key}: the runner's ctx, or ctx.twin. The bearer token is the runner's (the profile's selfhost-key.json), as in lib/mcp.mjs.
// Own fetch (not ctx.http) because the replay and wait=folded calls need longer timeouts than the client's default, and every message is redacted of the key.
import { readdirSync, statSync, existsSync } from 'node:fs'
import path from 'node:path'

export const XSD = 'http://www.w3.org/2001/XMLSchema#'
export const RDF_TYPE = 'http://www.w3.org/1999/02/22-rdf-syntax-ns#type'
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
const redact = (api, s) => (api.key ? String(s).replaceAll(api.key, '<key>') : String(s))

// ---- term and op builders (the payload shape of W7 section 3) --------------------------------------------------------
export const iri = (v) => ({ '@id': v })
export const lit = (value, type) => ({ '@value': String(value), '@type': type })
export const langLit = (value, lang) => ({ '@value': String(value), '@language': lang })
export const dateTime = (iso) => lit(iso, XSD + 'dateTime')
export const set = (subject, cls, predicate, value) => ({ op: 'set', subject, class: cls, predicate, value })
export const clear = (subject, cls, predicate) => ({ op: 'set', subject, class: cls, predicate, value: null })
export const add = (subject, cls, predicate, values) => ({ op: 'add', subject, class: cls, predicate, values })
export const remove = (subject, cls, predicate, values) => ({ op: 'remove', subject, class: cls, predicate, values })
export const retract = (subject, cls) => ({ op: 'retract', subject, class: cls })

// ---- plumbing ----------------------------------------------------------------------------------------------------------
// -> {status, ms, text, json}; never throws on an HTTP status, throws on a network error or timeout (message redacted).
export async function request(api, method, p, body, { timeoutMs = 600_000 } = {}) {
  const t0 = performance.now()
  let res, text
  try {
    res = await fetch(api.url.replace(/\/$/, '') + p, { method, headers: { 'content-type': 'application/json', accept: 'application/json', authorization: `Bearer ${api.key}` },
      body: body === undefined ? undefined : JSON.stringify(body), signal: AbortSignal.timeout(timeoutMs) })
    text = await res.text()
  } catch (e) { throw new Error(`${method} ${p}: ${redact(api, e.message)}`) }
  let json = null; try { json = JSON.parse(text) } catch { /* not json */ }
  return { status: res.status, ms: performance.now() - t0, text: redact(api, text), json }
}
const fail = (what, r) => new Error(`${what}: ${r.status} ${String(r.text).slice(0, 160)}`)
const path3 = (root, graph, vocab) => `/emporium/${root}/${encodeURIComponent(graph)}/${encodeURIComponent(vocab)}`

// ---- the commit seam ---------------------------------------------------------------------------------------------------
// commit(api, graph, vocab, body, {wait}) -> {status, ms, json, frames, committed, folded}. Throws on any status but 200, and on a 202 when wait asked for 'folded'
// and the fold did not cover the frames within the server's 10 s (the message says so). With {raw: true} returns the response whatever its status (for 4xx cases).
export async function commit(api, graph, vocab, body, { wait = false, raw = false, timeoutMs = 120_000 } = {}) {
  const r = await request(api, 'POST', path3('commit', graph, vocab) + (wait ? '?wait=folded' : ''), body, { timeoutMs })
  if (raw) return r
  if (r.status === 202 && wait) throw new Error(`commit ${graph}/${vocab}: 202, the fold did not cover the frames within the server's wait (${String(r.text).slice(0, 120)})`)
  if (r.status !== 200) throw fail(`commit ${graph}/${vocab}`, r)
  const j = r.json ?? {}
  if (wait && j.folded !== true) throw new Error(`commit ${graph}/${vocab}: wait=folded answered 200 with folded ${j.folded}`)
  return { status: r.status, ms: r.ms, json: j, frames: j.frames ?? [], committed: j.committed, folded: j.folded }
}
export const commitWatermark = async (api, graph, vocab) => {
  const r = await request(api, 'GET', path3('commit', graph, vocab) + '/watermark', undefined, { timeoutMs: 30_000 })
  if (r.status !== 200) throw fail(`watermark ${graph}/${vocab}`, r)
  return r.json
}

// ---- the fold routes -------------------------------------------------------------------------------------------------
// status -> the JSON, or null when the route 404s (a binary without the fold).
export async function foldStatus(api, graph, vocab) {
  const r = await request(api, 'GET', path3('fold', graph, vocab) + '/status', undefined, { timeoutMs: 30_000 })
  if (r.status === 404) return null
  if (r.status !== 200) throw fail(`fold status ${graph}/${vocab}`, r)
  return r.json
}
export async function foldHeads(api, graph, vocab, subject) {
  const r = await request(api, 'GET', path3('fold', graph, vocab) + `/heads?subject=${encodeURIComponent(subject)}`, undefined, { timeoutMs: 30_000 })
  if (r.status !== 200) throw fail(`fold heads ${graph}/${vocab}`, r)
  return r.json
}
// replay(api, graph, vocab, 'oracle' | 'live', timeoutMs) -> {json (events, triples, equal, missing, extra, ms, ...), ms (client wall)}
export async function replay(api, graph, vocab, into, timeoutMs = 900_000) {
  const r = await request(api, 'POST', path3('fold', graph, vocab) + '/replay', { into }, { timeoutMs })
  if (r.status >= 300) throw fail(`replay ${into} ${graph}/${vocab}`, r)
  return { json: r.json ?? {}, ms: r.ms }
}
const covered = (st) => st && (st.lag_frames ?? 0) === 0
// Poll the status route until lag_frames is 0 (the sink holds every closed frame). Returns ms from the call. Throws on timeout (with the last status).
export async function settle(api, graph, vocab, { timeoutMs = 600_000, pollMs = 50 } = {}) {
  const t0 = performance.now()
  for (;;) {
    const st = await foldStatus(api, graph, vocab)
    if (st === null) throw new Error(`fold status ${graph}/${vocab}: 404 (no fold on this binary)`)
    if (covered(st)) return performance.now() - t0
    if (performance.now() - t0 > timeoutMs) throw new Error(`fold ${graph}/${vocab} did not catch up in ${timeoutMs} ms (lag_frames ${st.lag_frames}, quarantined ${st.quarantined?.length ?? 0})`)
    await sleep(pollMs)
  }
}
// Poll the cheap watermark route until it covers the frame {origin, seq} (what a commit response returned). Returns ms from the call.
export async function waitCovered(api, graph, vocab, frame, { timeoutMs = 30_000, pollMs = 1 } = {}) {
  const t0 = performance.now()
  for (;;) {
    const w = await commitWatermark(api, graph, vocab)
    if ((w?.watermark?.[frame.origin] ?? 0) >= frame.seq) return performance.now() - t0
    if (performance.now() - t0 > timeoutMs) throw new Error(`fold watermark of ${graph}/${vocab} never covered ${frame.origin}#${frame.seq} in ${timeoutMs} ms`)
    await sleep(pollMs)
  }
}

// ---- reading the sink the way Nidra does ---------------------------------------------------------------------------------
// POST /api/sparql/query {graphId, sparql} -> rows of N-Triples terms. Patterns are wrapped in GRAPH ?g { ... } (an unwrapped pattern matches nothing).
export async function sparql(api, graphId, query) {
  const r = await request(api, 'POST', '/api/sparql/query', { graphId, sparql: query }, { timeoutMs: 300_000 })
  if (r.status >= 300 || r.json?.ok === false) throw fail(`sparql ${graphId}`, r)
  return r.json?.rows ?? []
}
const intOf = (term) => { const m = String(term ?? '').match(/^"?(-?\d+)/); return m ? Number(m[1]) : NaN }
// Triples whose subject starts with `prefix` in any graph of the store, counted by SPARQL (the sink is user:rdf for a contractless vocab).
export async function sinkCount(api, graphId, prefix = '') {
  const filter = prefix ? ` FILTER(STRSTARTS(STR(?s), "${prefix.replace(/["\\]/g, '')}"))` : ''
  const rows = await sparql(api, graphId, `SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o${filter} } }`)
  return intOf(rows[0]?.n)
}
// POST /api/sparql/update {graphId, sparql}. Used to plant a foreign triple in user:rdf (Nidra's vector-index rows are such) and prove a replay keeps it.
export async function sparqlUpdate(api, graphId, update) {
  const r = await request(api, 'POST', '/api/sparql/update', { graphId, sparql: update }, { timeoutMs: 300_000 })
  if (r.status >= 300 || r.json?.ok === false) throw fail(`sparql update ${graphId}`, r)
  return r.json
}
// The values of one (subject, predicate) in the sink, as N-Triples strings.
export async function sinkValues(api, graphId, subject, predicate) {
  const rows = await sparql(api, graphId, `SELECT ?o WHERE { GRAPH ?g { <${subject}> <${predicate}> ?o } }`)
  return rows.map((r) => r.o)
}

// ---- bytes on disk -----------------------------------------------------------------------------------------------------
// Sum of file sizes under <profile>/graphs/<graph>/ledger/<vocab> (the ledger directory of W5: <origin>.log files, the origin file). null when absent.
export function dirBytes(dir) {
  if (!existsSync(dir)) return null
  let n = 0
  const walk = (d) => { for (const e of readdirSync(d, { withFileTypes: true })) { const f = path.join(d, e.name); if (e.isDirectory()) walk(f); else if (e.isFile()) n += statSync(f).size } }
  walk(dir)
  return n
}
export const ledgerDir = (profile, graph, vocab) => path.join(profile, 'graphs', graph, 'ledger', vocab)
export const ledgerBytes = (profile, graph, vocab) => (profile ? dirBytes(ledgerDir(profile, graph, vocab)) : null)
// Bytes once they stop growing (the barrier trails the acks by up to the 1 s window; the ship task may still write).
export async function settledLedgerBytes(profile, graph, vocab, { maxMs = 8000, stepMs = 500 } = {}) {
  let prev = null, cur = ledgerBytes(profile, graph, vocab); const t0 = performance.now()
  while (cur !== null && cur !== prev && performance.now() - t0 < maxMs) { prev = cur; await sleep(stepMs); cur = ledgerBytes(profile, graph, vocab) }
  return cur
}
