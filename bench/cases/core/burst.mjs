import { IT, small, SMOKE } from './_util.mjs'
export const cases = [
  { id: 'burst.insert_blocks.same_doc_x8', version: 1, kind: 'burst', suites: SMOKE, params: IT([3, 1], [15, 2], 8), needs: ['small'],
    op: (ctx, i, k) => ctx.mcp('insert_blocks', { graphId: ctx.graphId, documentId: small(ctx, 0).id, content: `gbench burst ${ctx.tag} ${i}.${k}`, format: 'plain' }) },
  { id: 'burst.insert_blocks.distinct_docs_x8', version: 1, kind: 'burst', suites: SMOKE, params: IT([3, 1], [15, 2], 8), needs: ['small'],
    op: (ctx, i, k) => ctx.mcp('insert_blocks', { graphId: ctx.graphId, documentId: small(ctx, 10 + k).id, content: `gbench burst ${ctx.tag} ${i}.${k}`, format: 'plain' }) },
  // read_document.small latency while 8 writers loop edit_block_text on distinct docs (writers are background work, not samples)
  { id: 'burst.read_under_write', version: 1, kind: 'burst', suites: SMOKE, params: { ...IT([15, 2], [80, 5]), writers: 8 }, needs: ['small'],
    async during(ctx, signal) {
      const stats = { writerOps: 0, writerErrors: 0 }; ctx.shared.ruwStats = stats
      await Promise.all(Array.from({ length: 8 }, async (_, k) => { const d = small(ctx, 20 + k); const c = ctx.fork()
        while (!signal.stop) { try { await c.mcp('edit_block_text', { graphId: ctx.graphId, documentId: d.id, blockId: d.blockIds[1], operations: [{ type: 'insert', offset: 0, text: 'w' }] }) } catch { stats.writerErrors++ } stats.writerOps++ } }))
    },
    warmupDelayMs: 500,
    op: (ctx, i) => ctx.mcp('read_document', { graphId: ctx.graphId, documentId: small(ctx, i).id }),
    finish: (ctx) => ({ ...ctx.shared.ruwStats }) },
]
