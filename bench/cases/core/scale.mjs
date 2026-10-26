import { SMOKE } from './_util.mjs'
import { slope } from '../../lib/stats.mjs'
// Per-op latency of creating documents 1..N in a graph (a fresh graph in spawn mode). A slope in ms per 100 docs exposes O(N) per op.
export const cases = [{
  id: 'scale.create_documents', version: 1, kind: 'scale', suites: SMOKE, params: { iterations: 200, warmup: 0, concurrency: 1, smoke: { iterations: 20, warmup: 0 } }, needs: [],
  async setup(ctx) {
    if (ctx.mode === 'spawn') { const g = await ctx.http('POST', '/api/graphs', { title: `scale-${ctx.tag}`, graph_id: `scale-${ctx.tag}` }); if (g.status >= 300) throw new Error('scale graph ' + g.text.slice(0, 100)); ctx.graphId = `scale-${ctx.tag}` }
  },
  op: (ctx, i) => ctx.mcp('create_document', { graphId: ctx.graphId, title: `scale doc ${i}`, documentId: `gb-${ctx.tag}-sc${i}`, ...(ctx.fixture.runFolder ? { parentId: ctx.fixture.runFolder } : {}) }),
  finish: (ctx, samples) => ({ slope: { msPerHundredDocs: Math.round(slope(samples) * 100 * 100) / 100 } }),
}]
