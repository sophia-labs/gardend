import { IT, SUITES, graphFor, waitJob, phasedRun, stub } from './_rich.mjs'
import { markdownDoc, memoryRecords } from '../../fixture/parts/rich-gen.mjs'

const seed = (ctx) => ctx.fixture.seed ?? 1
const mk = (id, it, extra) => ({ id, version: 1, kind: 'rich', suites: SUITES, params: IT(...it), needs: [], ...extra })
const NO_MODEL = process.env.GBENCH_SKIP_MODEL ? 'GBENCH_SKIP_MODEL is set (the default model, bge-small-en-v1.5 quantized, is tens of MB; nomic v2 moe is 1.8 GB and is never chosen)' : null
// n paragraphs in one document, then the model prepared (a no-op after the first time on a machine).
async function corpus(ctx, name, n) {
  await graphFor(ctx, name)
  const doc = `sem-${ctx.tag}`; await ctx.mcp('create_document', { graphId: ctx.graphId, title: 'semantic corpus', documentId: doc })
  const paras = markdownDoc(n, seed(ctx)).split('\n\n').slice(1)
  for (let s = 0; s < paras.length; s += 100) await ctx.mcp('insert_blocks', { graphId: ctx.graphId, documentId: doc, content: paras.slice(s, s + 100).join('\n\n'), format: 'markdown' })
  await waitJob(ctx, await ctx.mcp('prepare_semantic_model', { graphId: ctx.graphId }))
  return doc
}
const gated = (def) => NO_MODEL ? { ...def, setup: undefined, op: undefined, run: async () => ({ skipped: NO_MODEL }) } : def

export const cases = [
  // Prepare the configured embedding model for a graph. The first call on a machine may download weights; later calls only load them.
  gated(mk('rich.semantic.prepare_model', [[1, 0], [1, 0]], { async setup(ctx) { await graphFor(ctx, 'sp') }, op: async (ctx) => waitJob(ctx, await ctx.mcp('prepare_semantic_model', { graphId: ctx.graphId })) })),
  // Build the index from nothing: 60 vs 300 paragraphs (embeds every block on CPU). One build per graph, so iterations are separate graphs' worth: n=1.
  ...[60, 300].map((n) => gated({ id: `rich.semantic.index_cold.b${n}`, version: 1, kind: 'rich', suites: SUITES, params: IT([1, 0], [1, 0]), needs: [],
    async setup(ctx) { ctx.S = { doc: await corpus(ctx, 'six' + n, n) } }, op: async (ctx) => waitJob(ctx, await ctx.mcp('refresh_semantic_index', { graphId: ctx.graphId })) })),
  // Incremental refresh after an edit: untimed, 5 new paragraphs; timed, refresh_semantic_index + job wait. Index of 300 blocks.
  gated({ id: 'rich.semantic.index_refresh.b300', version: 1, kind: 'rich', suites: SUITES, params: IT([2, 0], [6, 1]), needs: [],
    async setup(ctx) { ctx.S = { doc: await corpus(ctx, 'sir', 300) }; await waitJob(ctx, await ctx.mcp('refresh_semantic_index', { graphId: ctx.graphId })) },
    run: (ctx) => phasedRun(ctx, ctx.params, (c, i) => c.mcp('insert_blocks', { graphId: c.graphId, documentId: c.S.doc, content: markdownDoc(5, seed(c) + 100 + i).split('\n\n').slice(1).join('\n\n'), format: 'markdown' }), async (c) => waitJob(c, await c.mcp('refresh_semantic_index', { graphId: c.graphId }))) }),
  gated(mk('rich.semantic.search.b300', [[5, 1], [30, 3]], { async setup(ctx) { await corpus(ctx, 'sse', 300); await waitJob(ctx, await ctx.mcp('refresh_semantic_index', { graphId: ctx.graphId })) },
    op: (ctx, i) => ctx.mcp('semantic_search', { graphId: ctx.graphId, query: ['kalo mirentu', 'sel orva tos', 'lumfey', 'bar quin dri'][i % 4], limit: 10 }) })),
  // Memory: remember (one record), remember_batch (100), recall by substring and memory_semantic_recall over 1,000 memories.
  mk('rich.memory.remember', [[5, 1], [30, 3]], { async setup(ctx) { await graphFor(ctx, 'mr') }, op: (ctx, i) => ctx.mcp('remember', { graphId: ctx.graphId, ...memoryRecords(1, seed(ctx), i)[0] }) }),
  mk('rich.memory.remember_batch.b100', [[3, 1], [12, 2]], { async setup(ctx) { await graphFor(ctx, 'mrb') }, op: (ctx, i) => ctx.mcp('remember_batch', { graphId: ctx.graphId, records: memoryRecords(100, seed(ctx), i * 100) }) }),
  ...[50, 1000].map((m) => mk(`rich.memory.recall.m${m}`, [[5, 1], [30, 3]], { async setup(ctx) { await graphFor(ctx, 'mre' + m); for (let s = 0; s < m; s += 100) await ctx.mcp('remember_batch', { graphId: ctx.graphId, records: memoryRecords(Math.min(100, m - s), seed(ctx), s) }) },
    op: (ctx, i) => ctx.mcp('recall', { graphId: ctx.graphId, query: ['kalo', 'mi', 'sel', 'tu'][i % 4], limit: 20 }) })),
  ...[50, 1000].map((m) => gated(mk(`rich.memory.semantic_recall.m${m}`, [[3, 1], [20, 2]], { async setup(ctx) { await graphFor(ctx, 'msr' + m); await waitJob(ctx, await ctx.mcp('prepare_semantic_model', { graphId: ctx.graphId })); for (let s = 0; s < m; s += 100) await ctx.mcp('remember_batch', { graphId: ctx.graphId, records: memoryRecords(Math.min(100, m - s), seed(ctx), s) }) },
    op: (ctx, i) => ctx.mcp('memory_semantic_recall', { graphId: ctx.graphId, query: ['kalo mirentu', 'sel orva', 'tos fey'][i % 3], k: 10 }) }))),
  stub('rich.semantic.reason', 'STUB: semantic_reason needs headIri or tailIri plus a relationIri from the prepared scaffold; no synthetic relation scaffold is defined yet'),
]
