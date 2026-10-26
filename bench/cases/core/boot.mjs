// Process spawn to /health, to the first read, to the first SPARQL. Each rep restarts gardend on the same profile; the last rep stays up.
import { SMOKE } from './_util.mjs'
async function drive(ctx) {
  if (ctx.shared.boot) return ctx.shared.boot
  const reps = ctx.params.iterations, out = { cold_start: [], first_read: [], first_sparql: [] }, callMs = { first_read: [], first_sparql: [] }
  for (let rep = 0; rep < reps; rep++) {
    const g = await ctx.proc.start(), t0 = g.t0
    out.cold_start.push({ ms: await g.waitHealthy(), ok: true, traceId: null })
    const c = g.client()
    let r = await c.call('read_document', { graphId: ctx.graphId, documentId: ctx.fixture.book.id }); out.first_read.push({ ms: performance.now() - t0, ok: r.ok, traceId: r.traceId }); callMs.first_read.push(r.ms)
    r = await c.call('sparql_query', { graphId: ctx.graphId, query: 'SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } }' }); out.first_sparql.push({ ms: performance.now() - t0, ok: r.ok, traceId: r.traceId }); callMs.first_sparql.push(r.ms)
    ctx.log({ boot: rep + 1, cold: Math.round(out.cold_start.at(-1).ms), first_read: Math.round(out.first_read.at(-1).ms), first_sparql: Math.round(out.first_sparql.at(-1).ms) })
    if (rep < reps - 1) await ctx.proc.stop()
  }
  return (ctx.shared.boot = { out, callMs })
}
const def = { cold_start: 'process spawn to /health 200', first_read: 'process spawn to first read_document of the book ok', first_sparql: 'process spawn to first sparql count ok (after first_read)' }
export const cases = Object.keys(def).map((n) => ({
  id: `boot.${n}`, version: 1, kind: 'boot', suites: SMOKE, params: { iterations: 3, warmup: 0, concurrency: 1, def: def[n] }, needs: ['proc', 'book'],
  async run(ctx) { const { out, callMs } = await drive(ctx); return { calls: out[n], extra: callMs[n] ? { callMs: callMs[n].map((x) => Math.round(x * 10) / 10) } : undefined } },
}))
