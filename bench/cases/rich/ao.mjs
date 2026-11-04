// rich.ledger.ao.*: the Auto Ontology (Nidra) workload on the commit seam (W7 section 6, bench-ao.md section 4).
// Nidra's single writer sends Direct-Mapping rows as `set` ops to POST /emporium/commit/{graph}/ao; the vocab `ao` has NO contract (structural gate only),
// so the fold's sink is {graph_subject}:user:rdf and Nidra's GRAPH ?g { ... } SPARQL reads it. Cases, in order, share one graph and one tally:
//   sweep      the catalog sweep: 26,000 rows (1,000 tables x 26 rows) as set ops in requests of 1,000 rows, 90% value-equal re-asserts of the prior sweep, 10% changed.
//              The prior sweep (S0, all rows new) is setup; the timed sweep (S1) is one pass. Per-request ack ms are the samples. Gate: sink count == the writer's tally.
//   append     25,000 per-column profile updates, one request each (3 set ops), ack ms per request.             threshold append.p50
//   fold_lag   the question-time hot write (a sql_query row and its counter, 4 ops), ms from the ack until the fold's watermark covers the frame.   threshold fold.lag.p50
//   rebuild    live replay of the sink via the fold's replay route, then the gate again, an oracle replay, and a foreign triple that must survive.   threshold rebuild.ms
//   sync       (--twin) cold relay of the whole ledger from cell A to cell B, then cell B's own fold must hold the same tally.               threshold sync.ms
// Tiers: s = 100 tables (2,600 rows, 2,500 profile updates), m and l = 1,000 tables (26,000 rows, 25,000 updates), the sizes of W5.md section 318.
// Row shape: 9 `set` ops per row (name, dataType, nullable, ordinal, comment, distinctCount, nullFraction, sampleCount, and #ref-table / #schema), so a subject folds to 1 rdf:type + 9 heads
// = 10 triples and 26,000 rows are 260,000 triples (the W5 estimate was about 250,000). The writer's tally is rows x 10 plus the hot rows (4 each) and the counter (2).
// Thresholds are PROPOSED (bench-ao.md section 4, "needs Vera"); the two hard gates (sink count == tally, foreign triple kept) are not.
// yjs / y-protocols are not needed (the sync case uses lib/ledger.mjs, which needs @noble/hashes only).
import { IT, SUITES, graphFor, phasedRun, sleep } from './_rich.mjs'
import { tierOf } from './_log.mjs'
import * as ledger from '../../lib/ledger.mjs'
import * as cm from '../../lib/commit.mjs'

const VOCAB = 'ao'
const NS = 'urn:ao:'
const WITNESS = 'user-bench'
const WRITER = 'nidra-bench-worker'
const COLS = 25
const PER_TABLE = COLS + 1
const CHUNK_ROWS = 1000
const PROPS = 9
const DT = ['varchar', 'integer', 'numeric', 'timestamp', 'boolean', 'text', 'date']
const NO_FOLD = 'no ledger/fold on this binary (version route 404); nothing to measure'
const NO_DEPS = '@noble/hashes not installed (pnpm install in bench/)'
const TIMEOUT = 300_000
const r2 = (x) => Math.round(x * 100) / 100
const tablesOf = (ctx) => (tierOf(ctx) === 's' ? 100 : 1000)
const updatesOf = (ctx) => (tierOf(ctx) === 's' ? 2500 : 25000)

// ---- the rows ----------------------------------------------------------------------------------------------------------------
const tableSubject = (t) => `${NS}table/id=t${t}`
const columnSubject = (t, c) => `${NS}column/id=t${t}c${c}`
// Row k of the catalog: k % 26 === 0 is its table, the other 25 are the table's columns. gen 0 is the prior sweep; at gen >= 1 every 10th row has a new dataType.
function rowOps(k, gen) {
  const t = Math.floor(k / PER_TABLE), c = k % PER_TABLE
  const isTable = c === 0, cls = NS + (isTable ? 'table' : 'column'), s = isTable ? tableSubject(t) : columnSubject(t, c - 1)
  const changed = gen >= 1 && k % 10 === 0
  const p = (n) => `${cls}#${n}`
  return [
    cm.set(s, cls, p('name'), isTable ? `table_${t}` : `col_${t}_${c - 1}`),
    cm.set(s, cls, p('dataType'), DT[(k + (changed ? 1 : 0)) % DT.length]),
    cm.set(s, cls, p('nullable'), k % 3 === 0),
    cm.set(s, cls, p('ordinal'), c),
    cm.set(s, cls, p('comment'), `row ${k} of the catalog sweep`),
    cm.set(s, cls, p('distinctCount'), (k * 31) % 9973),
    cm.set(s, cls, p('nullFraction'), (k % 100) / 100),
    cm.set(s, cls, p('sampleCount'), 1000 + (k % 500)),
    isTable ? cm.set(s, cls, p('schema'), cm.iri(`${NS}schema/id=public`)) : cm.set(s, cls, p('ref-table'), cm.iri(tableSubject(t))),
  ]
}
const bodyOf = (ops) => ({ witness: WITNESS, writer: WRITER, ops })
const colAt = (i, tables) => ({ t: i % tables, c: (i * 13) % COLS })

// ---- shared state: one graph, one tally -----------------------------------------------------------------------------------------
async function prepare(ctx) {
  const A = (ctx.shared.ao ??= {})
  if (!A.g) {
    A.g = await graphFor(ctx, 'ao')
    A.tables = tablesOf(ctx); A.rows = A.tables * PER_TABLE; A.tally = 0; A.hot = 0; A.noFold = (await ledger.version(ctx, A.g, VOCAB)) === null
  }
  ctx.graphId = A.g; ctx.noFold = A.noFold
  if (!A.noFold && !A.s0) { // the prior sweep S0: every row new. Acked in requests, then the fold must catch up; the tally is exact.
    const t0 = performance.now()
    for (let k = 0; k < A.rows; k += CHUNK_ROWS) await sweepChunk(ctx, A, k, 0)
    A.s0AckMs = Math.round(performance.now() - t0); A.s0FoldMs = Math.round(await cm.settle(ctx, A.g, VOCAB, { timeoutMs: TIMEOUT }))
    A.tally = A.rows * (1 + PROPS); A.s0 = true
    ctx.log({ ao_s0: A.g, rows: A.rows, ackMs: A.s0AckMs, foldMs: A.s0FoldMs })
  }
  ctx.params.n_actual = A.rows; ctx.params.n_unit = 'catalog rows (9 set ops each)'; ctx.params.tier = tierOf(ctx)
  return A
}
async function sweepChunk(ctx, A, k0, gen) {
  const ops = []
  for (let k = k0; k < Math.min(A.rows, k0 + CHUNK_ROWS); k++) ops.push(...rowOps(k, gen))
  return cm.commit(ctx, A.g, VOCAB, bodyOf(ops), { timeoutMs: 300_000 })
}
const meta = (A, extra = {}) => ({ extra: { rows: A.rows, tables: A.tables, tally: A.tally, noFold: !!A.noFold, notes: [`${A.rows} catalog rows over ${A.tables} tables; prior sweep acked in ${A.s0AckMs} ms, folded ${A.s0FoldMs} ms later`], ...extra } })
const mk = (id, it, extra) => ({ id, version: 1, kind: 'rich', suites: SUITES, params: IT(...it), needs: [], ...extra })
const gateCount = async (ctx, A) => { const n = await cm.sinkCount(ctx, A.g, NS); return { sinkCount: n, equal: n === A.tally } }

const T = {
  sinkEqual: { name: 'sink count == writer tally (hard gate)', metric: 'extra.equal', op: '==', limit: true },
  appendP50: { name: 'append.p50', metric: 'p50', op: '<=', limit: 60, unit: 'ms', proposed: true, note: 'W4 floor was about 48 ms (SLICE-1 section 12)' },
  foldLag: { name: 'fold.lag.p50', metric: 'p50', op: '<=', limit: 250, unit: 'ms', proposed: true, note: 'W4 sync.delta p50 <= 250 ms' },
  rebuild: { name: 'rebuild.ms', metric: 'p50', op: '<=', limit: 60_000, unit: 'ms', proposed: true, note: 'W4 replay_live <= 60 s; 26k rows here' },
  sync: { name: 'sync.ms', metric: 'p50', op: '<=', limit: 5000, unit: 'ms', proposed: true, note: 'W4 sync.n10k <= 5 s; the sweep is tens of thousands of frames here, so scale-up is expected and this is flagged' },
}

export const cases = [
  // The catalog sweep: one pass over every row, 90% value-equal re-asserts. Samples are the per-request ack times; the gate is the sink count after the fold catches up.
  mk('rich.ledger.ao.sweep', [[1, 0], [1, 0]], {
    thresholds: [T.sinkEqual, { name: 'changed rows visible, re-asserted rows unchanged', metric: 'extra.valuesOk', op: '==', limit: true }],
    async setup(ctx) { await prepare(ctx) },
    async run(ctx) {
      const A = ctx.shared.ao
      if (A.noFold) return { skipped: NO_FOLD }
      const calls = []; const t0 = performance.now()
      for (let k = 0; k < A.rows; k += CHUNK_ROWS) {
        const t1 = performance.now(); let ok = true, error = null
        try { await sweepChunk(ctx, A, k, 1) } catch (e) { ok = false; error = String(e.message).slice(0, 200) }
        calls.push({ ms: performance.now() - t1, ok, traceId: null, error })
      }
      const ackMs = Math.round(performance.now() - t0), foldMs = Math.round(await cm.settle(ctx, A.g, VOCAB, { timeoutMs: TIMEOUT }))
      ctx.params.iterations = calls.length; ctx.params.warmup = 0
      const gate = await gateCount(ctx, A)
      // row 0 (the first table) is changed at gen 1 (k % 10 === 0): its dataType is now DT[1]; row 3 (column 2 of table 0) is a re-assert and keeps DT[3].
      const v0 = await cm.sinkValues(ctx, A.g, tableSubject(0), `${NS}table#dataType`), v3 = await cm.sinkValues(ctx, A.g, columnSubject(0, 2), `${NS}column#dataType`)
      const valuesOk = v0.length === 1 && v0[0].includes(DT[1]) && v3.length === 1 && v3[0].includes(DT[3])
      return { calls, ...meta(A, { ...gate, valuesOk, sweepAckMs: ackMs, foldDrainMs: foldMs, changedRows: Math.ceil(A.rows / 10), equalRows: A.rows - Math.ceil(A.rows / 10),
        chunks: calls.length, notes: [`sweep of ${A.rows} rows in ${calls.length} requests: acked in ${ackMs} ms, fold drained ${foldMs} ms later; 10% of rows changed, 90% value-equal re-asserts`, `sink triples ${gate.sinkCount}, tally ${A.tally}`] }) }
    } }),

  // 25k per-column profile updates, one request and one frame each, acked on the barrier (no wait).
  mk('rich.ledger.ao.append', [[100, 10], [25000, 50]], {
    thresholds: [T.appendP50, T.sinkEqual],
    async setup(ctx) { const A = await prepare(ctx); ctx.params.iterations = updatesOf(ctx); ctx.params.warmup = 50; ctx.params.n_actual = ctx.params.iterations; ctx.params.n_unit = 'profile updates (3 set ops each, one request each)'; return A },
    async run(ctx) {
      const A = ctx.shared.ao
      if (A.noFold) return { skipped: NO_FOLD }
      const out = await phasedRun(ctx, ctx.params, async (c, i) => {
        const { t, c: col } = colAt(i, A.tables), s = columnSubject(t, col), cls = NS + 'column', p = (n) => `${cls}#${n}`
        c._body = bodyOf([cm.set(s, cls, p('distinctCount'), 100_000 + i), cm.set(s, cls, p('nullFraction'), (i % 100) / 100), cm.set(s, cls, p('sampleCount'), 2_000_000 + i)])
      }, async (c) => { await cm.commit(c, A.g, VOCAB, c._body) })
      const foldMs = Math.round(await cm.settle(ctx, A.g, VOCAB, { timeoutMs: TIMEOUT })), gate = await gateCount(ctx, A)
      return { calls: out.calls, ...meta(A, { ...gate, foldDrainMs: foldMs, notes: [`${ctx.params.iterations + ctx.params.warmup} single-column profile updates; fold drained ${foldMs} ms after the last ack; sink ${gate.sinkCount}, tally ${A.tally}`] }) }
    } }),

  // The question-time hot write: a sql_query row and a counter in one request. The sample is the time from the ack until the fold's watermark covers the frame
  // (the status route's lag as a client sees it); one more write at the end uses ?wait=folded and reports its total.
  mk('rich.ledger.ao.fold_lag', [[10, 2], [200, 10]], {
    thresholds: [T.foldLag, T.sinkEqual],
    async setup(ctx) { await prepare(ctx) },
    async run(ctx) {
      const A = ctx.shared.ao
      if (A.noFold) return { skipped: NO_FOLD }
      const calls = [], ack = [], stamp = Date.now(), cc = NS + 'counter', ctr = `${NS}counter/id=sql_query`, cls = NS + 'sql_query'
      const body = (tag, i) => { const q = `${NS}sql_query/id=q${stamp}-${tag}${i}`; return { q, body: bodyOf([cm.set(q, cls, cls + '#text', `SELECT count(*) FROM t${i}`), cm.set(q, cls, cls + '#askedAtMs', Date.now()), cm.set(q, cls, cls + '#question', `how many rows are in t${i}`), cm.set(ctr, cc, cc + '#count', i)]) } }
      const tally = () => { A.hot += 1; A.tally += 4 + (A.hotCounter ? 0 : 2); A.hotCounter = true } // a new query row (type + 3 heads) and, once, the counter (type + count)
      let last = null
      for (let i = 0; i < ctx.params.iterations + ctx.params.warmup; i++) {
        const { q, body: b } = body('', i); let ok = true, error = null, ms = 0
        try {
          const r = await cm.commit(ctx, A.g, VOCAB, b); tally(); last = q
          const frame = r.frames.at(-1); if (!frame) throw new Error('commit response carried no frame')
          ms = await cm.waitCovered(ctx, A.g, VOCAB, frame)
          if (i >= ctx.params.warmup) ack.push(Math.round(r.ms * 10) / 10)
        } catch (e) { ok = false; error = String(e.message).slice(0, 200) }
        if (i >= ctx.params.warmup) calls.push({ ms, ok, traceId: null, error })
      }
      // the same write with ?wait=folded: ack and fold in one number
      const w = body('w', 0), t0 = performance.now(); let waitFoldedMs = null
      try { await cm.commit(ctx, A.g, VOCAB, w.body, { wait: true }); waitFoldedMs = Math.round(performance.now() - t0); tally(); last = w.q } catch (e) { waitFoldedMs = `error: ${String(e.message).slice(0, 120)}` }
      const foldMs = Math.round(await cm.settle(ctx, A.g, VOCAB, { timeoutMs: TIMEOUT })), gate = await gateCount(ctx, A)
      const seen = last ? await cm.sinkValues(ctx, A.g, last, cls + '#text') : []
      return { calls, ...meta(A, { ...gate, ackMs: ack, waitFoldedMs, lastRowVisible: seen.length === 1, foldDrainMs: foldMs, hotRows: A.hot,
        notes: [`${calls.length} hot writes: sample = ack to watermark-covers-frame; the ?wait=folded write took ${waitFoldedMs} ms in all; sink ${gate.sinkCount}, tally ${A.tally}`] }) }
    } }),

  // Rebuild the sink from the ledger (the admin act): live replay of the whole vocab. Afterwards: the gate again, an oracle replay, and a foreign triple planted in user:rdf
  // before the rebuild must still be there (the fold owns only its own subjects).
  mk('rich.ledger.ao.rebuild', [[1, 0], [1, 0]], {
    thresholds: [T.rebuild, T.sinkEqual, { name: 'oracle replay equal after the rebuild', metric: 'extra.oracleEqual', op: '==', limit: true }, { name: 'foreign user:rdf triple survives the live replay', metric: 'extra.foreignKept', op: '==', limit: true }],
    async setup(ctx) { await prepare(ctx) },
    async run(ctx) {
      const A = ctx.shared.ao
      if (A.noFold) return { skipped: NO_FOLD }
      const sink = `urn:mnemosyne:local:graph:${A.g}:user:rdf`, foreign = 'urn:gbench:foreign:vector-row-1'
      let planted = true
      try { await cm.sparqlUpdate(ctx, A.g, `INSERT DATA { GRAPH <${sink}> { <${foreign}> <urn:gbench:foreign#p> "kept" } }`) } catch { planted = false }
      const calls = []; let rep = null, ok = true, error = null
      const t0 = performance.now()
      try { rep = await cm.replay(ctx, A.g, VOCAB, 'live', 900_000); if (rep.json.equal === false) throw new Error(`live replay not equal: missing ${rep.json.missing} extra ${rep.json.extra}`) } catch (e) { ok = false; error = String(e.message).slice(0, 200) }
      const ms = rep ? (rep.json.ms ?? rep.ms) : performance.now() - t0
      calls.push({ ms, ok, traceId: null, error })
      ctx.params.iterations = 1; ctx.params.warmup = 0
      const settleMs = Math.round(await cm.settle(ctx, A.g, VOCAB, { timeoutMs: TIMEOUT })), gate = await gateCount(ctx, A)
      const kept = planted ? (await cm.sinkCount(ctx, A.g, foreign)) === 1 : null
      let oracle = null
      try { oracle = (await cm.replay(ctx, A.g, VOCAB, 'oracle', 900_000)).json } catch (e) { oracle = { error: String(e.message).slice(0, 160) } }
      return { calls, ...meta(A, { ...gate, serverMs: rep?.json.ms, clientMs: rep ? Math.round(rep.ms) : null, events: rep?.json.events, triples: rep?.json.triples, settleMs,
        foreignPlanted: planted, foreignKept: kept, oracleEqual: oracle?.equal === true, oracleReport: oracle ? { events: oracle.events, triples: oracle.triples, missing: oracle.missing, extra: oracle.extra, ms: oracle.ms, error: oracle.error } : null,
        notes: [`live replay ${ms} ms (events ${rep?.json.events}, triples ${rep?.json.triples}); sink ${gate.sinkCount}, tally ${A.tally}; foreign triple planted ${planted}, kept ${kept}`] }) }
    } }),

  // Cold sync of the whole ao ledger from cell A to cell B (the twin): a client subscribed to A relayed into a client on B, ms until B's version covers A's.
  // Then cell B's fold, which saw only the relayed frames, must hold the same tally (the second hard gate, on the replica).
  mk('rich.ledger.ao.sync', [[1, 0], [1, 0]], {
    needs: ['twin'], thresholds: [T.sync, { name: 'replica sink count == writer tally (hard gate)', metric: 'extra.twinEqual', op: '==', limit: true }],
    async setup(ctx) { await prepare(ctx) },
    async run(ctx) {
      if (!ledger.available) return { skipped: NO_DEPS }
      const A = ctx.shared.ao
      if (A.noFold) return { skipped: NO_FOLD }
      const g = A.g, r = await ctx.twin.http('POST', '/api/graphs', { title: g, graph_id: g })
      if (r.status >= 300 && !/exist|already/i.test(r.text)) throw new Error(`twin create graph ${g}: ${r.status}`)
      const av = await ledger.version(ctx, g, VOCAB)
      const want = Object.entries(av.origins ?? {}), total = ledger.sumOf(av.origins)
      let X = null, Y = null, ms = 0, ok = true, error = null, twinCount = null
      try {
        X = ledger.connect(ctx.url, ctx.key, g, VOCAB, `ax-${Math.random().toString(36).slice(2, 7)}`)
        Y = ledger.connect(ctx.twin.url, ctx.twin.key, g, VOCAB, `ay-${Math.random().toString(36).slice(2, 7)}`)
        await Promise.all([X.synced(), Y.synced()])
        const t1 = performance.now()
        for (;;) { if (want.every(([o, n]) => ledger.lastSeq(X, o) >= n)) break; if (performance.now() - t1 > TIMEOUT) throw new Error('client X never held the ledger of cell A'); await sleep(10) }
        const t0 = performance.now(), off = ledger.relay(X, Y)
        try { await ledger.waitVersion(ctx.twin, g, VOCAB, (v) => want.every(([o, n]) => (v.origins?.[o] ?? 0) >= n), { timeoutMs: TIMEOUT }); ms = performance.now() - t0 } finally { off() }
        if (X.errors.length || Y.errors.length) throw new Error(`client errors: ${[...X.errors, ...Y.errors].slice(0, 2).join('; ')}`)
        await cm.settle(ctx.twin, g, VOCAB, { timeoutMs: TIMEOUT })
        twinCount = await cm.sinkCount(ctx.twin, g, NS)
      } catch (e) { ok = false; error = String(e.message).slice(0, 200); ms = TIMEOUT } finally { X?.close(); Y?.close() }
      ctx.params.iterations = 1; ctx.params.warmup = 0
      return { calls: [{ ms, ok, traceId: null, error }], ...meta(A, { frames: total, origins: want.length, twinSinkCount: twinCount, twinEqual: twinCount === A.tally,
        notes: [`cold relay of ${total} frames from ${want.length} origin(s): ${Math.round(ms)} ms (proposed limit 5 s is flagged: the sweep is tens of thousands of frames)`, `replica sink ${twinCount}, tally ${A.tally}`] }) }
    } }),
]
