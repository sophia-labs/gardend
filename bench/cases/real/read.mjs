// Reads on a CLONE of real data: `gbench run|ab --real-profile <backup dir> --graph <id> --suite real`. Only ctx.graphId is
// known (no fixture manifest). The runner refuses ~/sophia itself; point it at a backup (it is APFS-cloned per round, so
// every round starts from the same bytes, and the first read after boot sees the backup's projection state).
import { newTrace } from '../../lib/mcp.mjs'
const R = ['real'], IT = (n, w) => ({ iterations: n, warmup: w, concurrency: 1 })
const COUNT = 'SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } }'
async function firstDoc(ctx) {
  if (ctx.shared.firstDoc !== undefined) return ctx.shared.firstDoc
  const p = await ctx.mcp('list_documents', { graphId: ctx.graphId }), d = (p.documents ?? p.items ?? [])[0]
  ctx.shared.firstDoc = d ? { id: d.documentId ?? d.document_id ?? d.id, title: String(d.title ?? '') } : null
  return ctx.shared.firstDoc
}
export const cases = [
  // The first graph-wide SPARQL after a boot on this data: the read fence's first pass (production 10-26: "timed out after 30s").
  { id: 'real.first_sparql', version: 1, kind: 'boot', suites: R, params: IT(1, 0), needs: ['proc'],
    async run(ctx) {
      const g = await ctx.proc.start(); await g.waitHealthy(300_000)
      ctx._trace = newTrace(); const t0 = performance.now(); let ok = true, error = null
      try { await ctx.mcp('sparql_query', { graphId: ctx.graphId, query: COUNT }) } catch (e) { ok = false; error = String(e.message).slice(0, 160) }
      const ms = performance.now() - t0
      // a failed first read is the finding: record its time as the sample too, with the error
      return { calls: [{ traceId: ctx._trace.traceId, ms, ok: true, error: null }], extra: { firstReadOk: ok, ...(error ? { firstReadError: error } : {}) } }
    } },
  { id: 'real.sparql.count', version: 1, kind: 'read', suites: R, params: IT(5, 1), needs: [], op: (ctx) => ctx.mcp('sparql_query', { graphId: ctx.graphId, query: COUNT }) },
  { id: 'real.list_documents', version: 1, kind: 'read', suites: R, params: IT(6, 1), needs: [], op: (ctx) => ctx.mcp('list_documents', { graphId: ctx.graphId }) },
  { id: 'real.get_workspace', version: 1, kind: 'read', suites: R, params: IT(6, 1), needs: [], op: (ctx) => ctx.mcp('get_workspace', { graphId: ctx.graphId }) },
  { id: 'real.search_documents', version: 1, kind: 'read', suites: R, params: IT(6, 1), needs: [],
    async setup(ctx) { const d = await firstDoc(ctx); ctx.shared.q = (d?.title ?? '').trim().slice(0, 4) || 'a' },
    op: (ctx) => ctx.mcp('search_documents', { graphId: ctx.graphId, query: ctx.shared.q }) },
  { id: 'real.read_document', version: 1, kind: 'read', suites: R, params: IT(6, 1), needs: [],
    async setup(ctx) { if (!(await firstDoc(ctx))) throw new Error('graph has no documents') },
    op: (ctx) => ctx.mcp('read_document', { graphId: ctx.graphId, documentId: ctx.shared.firstDoc.id }) },
]
