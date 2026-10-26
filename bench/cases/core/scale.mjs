import { SMOKE } from './_util.mjs'
import { slope, median } from '../../lib/stats.mjs'
import { newTrace } from '../../lib/mcp.mjs'
// Per-op latency of creating documents 1..N in a graph (a fresh graph in spawn mode). A slope in ms per 100 docs exposes O(N) per op.
export const cases = [{
  id: 'scale.create_documents', version: 1, kind: 'scale', suites: SMOKE, params: { iterations: 150, warmup: 0, concurrency: 1, smoke: { iterations: 20, warmup: 0 } }, needs: [],
  async setup(ctx) {
    if (ctx.mode === 'spawn') { const g = await ctx.http('POST', '/api/graphs', { title: `scale-${ctx.tag}`, graph_id: `scale-${ctx.tag}` }); if (g.status >= 300) throw new Error('scale graph ' + g.text.slice(0, 100)); ctx.graphId = `scale-${ctx.tag}` }
  },
  op: (ctx, i) => ctx.mcp('create_document', { graphId: ctx.graphId, title: `scale doc ${i}`, documentId: `gb-${ctx.tag}-sc${i}`, ...(ctx.fixture.runFolder ? { parentId: ctx.fixture.runFolder } : {}) }),
  finish: (ctx, samples) => ({ slope: { msPerHundredDocs: Math.round(slope(samples) * 100 * 100) / 100 } }),
},
// create_folder against workspace size, two points: `per` folders in a fresh empty graph, then `per` in the fixture graph
// (N = its documents + folders). Samples are the fixture-graph calls (the cost at size N); slope = (median at N - median
// at 0) per 100 workspace entities. A flat slope means a workspace op no longer re-reconciles the whole workspace.
{
  id: 'scale.create_folder', version: 1, kind: 'scale', suites: SMOKE, params: { iterations: 8, warmup: 1, concurrency: 1, smoke: { iterations: 5, warmup: 1 } }, needs: ['small'],
  async run(ctx) {
    const n = ctx.params.iterations, w = ctx.params.warmup, N = (ctx.fixture.counts?.documents ?? 0) + (ctx.fixture.counts?.folders ?? 0)
    if (ctx.mode !== 'spawn' || !N) return { skipped: 'needs a spawned fixture with a known size' }
    const empty = `scalef-${ctx.tag}`, g = await ctx.http('POST', '/api/graphs', { title: empty, graph_id: empty }); if (g.status >= 300) throw new Error('scale graph ' + g.text.slice(0, 100))
    const time = async (graphId, i) => { ctx._trace = newTrace(); const t0 = performance.now(); await ctx.mcp('create_folder', { graphId, name: `gbench sf ${ctx.tag} ${i}`, folderId: `gb-${ctx.tag}-sf${i}` }); return { traceId: ctx._trace.traceId, ms: performance.now() - t0, ok: true } }
    const at0 = [], atN = []
    for (let i = 0; i < n + w; i++) { const r = await time(empty, i); if (i >= w) at0.push(r) }
    for (let i = 0; i < n + w; i++) { const r = await time(ctx.graphId, 100 + i); if (i >= w) atN.push(r) }
    ctx._trace = null
    const m0 = median(at0.map((c) => c.ms)), mN = median(atN.map((c) => c.ms))
    return { calls: atN, slope: { msPerHundredDocs: Math.round(((mN - m0) / N) * 100 * 100) / 100 }, extra: { emptyGraphP50Ms: Math.round(m0 * 10) / 10, workspaceEntities: N } }
  },
},
]
